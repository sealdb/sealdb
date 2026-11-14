use super::table::TableInfo;
use crate::model::job::{STATE_PUBLIC, SchemaState};
use sealdb_errno::{ERR_NO_PARTITION_FOR_GIVEN_VALUE, Error, Result};
use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
use sealdb_parser::model::CIStr;
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// TiDB `model.PartitionType` core cases for .
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PartitionType {
    Range,
    Hash,
    List,
    Key,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PartitionDefinition {
    pub id: i64,
    pub name: CIStr,
    /// RANGE bound as a decimal string, or `MAXVALUE`. Empty for HASH/KEY.
    #[serde(rename = "less_than", default)]
    pub less_than: Vec<String>,
    /// LIST `VALUES IN` values (`None` is SQL `NULL`). Empty for RANGE/HASH/KEY.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_values: Vec<Option<i64>>,
}

impl PartitionDefinition {
    pub fn new(id: i64, name: impl Into<String>, less_than: Vec<String>) -> Self {
        Self {
            id,
            name: CIStr::new(name),
            less_than,
            in_values: Vec::new(),
        }
    }

    pub fn with_in_values(mut self, values: Vec<Option<i64>>) -> Self {
        self.in_values = values;
        self
    }

    pub fn is_maxvalue(&self) -> bool {
        self.less_than
            .first()
            .is_some_and(|v| v.eq_ignore_ascii_case("MAXVALUE"))
    }

    pub fn bound(&self) -> Option<i64> {
        if self.is_maxvalue() {
            return None;
        }
        self.less_than.first()?.parse().ok()
    }
}

/// TiDB `model.PartitionInfo` core fields used by .
/// Completeness wave: nested subpartition storage, reorg states.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PartitionInfo {
    #[serde(rename = "type")]
    pub tp: PartitionType,
    pub expr: String,
    pub columns: Vec<CIStr>,
    pub enable: bool,
    pub definitions: Vec<PartitionDefinition>,
    #[serde(default)]
    pub num: u64,
    pub state: SchemaState,
}

impl PartitionInfo {
    pub fn new(
        tp: PartitionType,
        column: impl Into<String>,
        defs: Vec<PartitionDefinition>,
    ) -> Self {
        let column = column.into();
        let num = defs.len() as u64;
        Self {
            tp,
            expr: column.clone(),
            columns: vec![CIStr::new(column)],
            enable: true,
            definitions: defs,
            num,
            state: STATE_PUBLIC,
        }
    }

    pub fn with_columns(mut self, columns: Vec<String>) -> Self {
        if !columns.is_empty() {
            self.expr = columns.join(",");
            self.columns = columns.into_iter().map(CIStr::new).collect();
        }
        self
    }

    /// Scalar expression partition key (HASH/RANGE/LIST); clears `columns`.
    pub fn with_expr_key(mut self, expr: impl Into<String>) -> Self {
        self.expr = expr.into();
        self.columns.clear();
        self
    }

    /// True when locate must evaluate `expr` instead of reading a column.
    pub fn is_expr_key(&self) -> bool {
        self.columns.is_empty() && !self.expr.is_empty()
    }

    pub fn column(&self) -> &str {
        self.columns
            .first()
            .map(|c| c.o())
            .unwrap_or(self.expr.as_str())
    }

    pub fn physical_ids(&self) -> Vec<i64> {
        self.definitions.iter().map(|d| d.id).collect()
    }

    pub fn locate(&self, value: &Datum) -> Result<i64> {
        match self.tp {
            PartitionType::Range => self.locate_range(value),
            PartitionType::Hash => self.locate_hash(value),
            PartitionType::Key => self.locate_key(value),
            PartitionType::List => self.locate_list(value),
        }
    }

    fn locate_range(&self, value: &Datum) -> Result<i64> {
        if matches!(value, Datum::Null) {
            return self
                .definitions
                .first()
                .map(|d| d.id)
                .ok_or_else(no_partition);
        }
        for def in &self.definitions {
            if def.is_maxvalue() {
                return Ok(def.id);
            }
            let Some(bound) = def.less_than.first() else {
                return Err(no_partition());
            };
            if compare_bound(value, bound) == Some(Ordering::Less) {
                return Ok(def.id);
            }
        }
        Err(no_partition_value(value))
    }

    fn locate_hash(&self, value: &Datum) -> Result<i64> {
        if self.definitions.is_empty() {
            return Err(no_partition());
        }
        if matches!(value, Datum::Null) {
            return Ok(self.definitions[0].id);
        }
        // TiDB / MySQL HASH: `value % partitions`, then `abs` the signed
        // remainder, so negatives mirror on the first partitions
        // (`a = -1` with 4 partitions lands in p1). UNSIGNED values hash
        // through their signed bit pattern, matching TiDB.
        let v = match value {
            Datum::I64(v) => *v,
            Datum::Year(v) => *v,
            Datum::U64(v) => *v as i64,
            _ => return Err(no_partition()),
        };
        let n = self.definitions.len() as i64;
        let idx = (v % n).unsigned_abs();
        Ok(self.definitions[idx as usize].id)
    }

    /// TiDB `KEY` partitioning: MD5 of the key value, first 8 digest bytes as
    /// a big-endian `u64`, then `% partitions`. Distinct from HASH (modulo of
    /// the raw value), so `9223372036854775807` and `9223372036854775808`
    /// both land in `p2` of five.
    fn locate_key(&self, value: &Datum) -> Result<i64> {
        if self.definitions.is_empty() {
            return Err(no_partition());
        }
        if matches!(value, Datum::Null) {
            return Ok(self.definitions[0].id);
        }
        let idx = super::key_hash::key_partition_index(value, self.definitions.len())
            .ok_or_else(no_partition)?;
        Ok(self.definitions[idx].id)
    }

    /// Lexicographic RANGE COLUMNS compare: the first bound the row is strictly
    /// less than (MAXVALUE is +inf) is the partition.
    pub fn locate_tuple(&self, values: &[Datum]) -> Result<i64> {
        if self.columns.len() <= 1 {
            // Plain RANGE reports the offending value; LIST keeps TiDB's
            // column-list wording for a single key column.
            return match self.locate(values.first().unwrap_or(&Datum::Null)) {
                Ok(id) => Ok(id),
                Err(e) if self.tp == PartitionType::Range => Err(e),
                Err(_) => Err(no_partition()),
            };
        }
        for def in &self.definitions {
            if tuple_less(values, &def.less_than) {
                return Ok(def.id);
            }
        }
        Err(no_partition())
    }

    fn locate_list(&self, value: &Datum) -> Result<i64> {
        // LIST partitions accept only the declared values; `NULL` matches an
        // explicit `NULL` list entry (TiDB `VALUES IN (…, NULL)`).
        let want = match value {
            Datum::Null => None,
            other => datum_i64(other).map(Some).ok_or_else(no_partition)?,
        };
        self.definitions
            .iter()
            .find(|def| def.in_values.contains(&want))
            .map(|def| def.id)
            .ok_or_else(|| no_partition_value(value))
    }

    pub fn prune(&self, selection: Option<&ExprNode>) -> Vec<i64> {
        match selection {
            None => self.physical_ids(),
            Some(expr) => self.prune_expr(expr).unwrap_or_else(|| self.physical_ids()),
        }
    }

    fn prune_expr(&self, expr: &ExprNode) -> Option<Vec<i64>> {
        // Folded boolean constants from predicate simplification.
        if let ExprNode::Value(_) = expr {
            return match const_datum(expr)? {
                Datum::I64(1) => Some(self.physical_ids()),
                Datum::I64(0) => Some(Vec::new()),
                _ => None,
            };
        }
        // `col IS NULL`: TiDB locates NULL to the first partition for every
        // partitioning type (range NULL sorts lowest; hash(NULL) is 0).
        if let ExprNode::IsNull(isnull) = expr {
            if isnull.not {
                return None;
            }
            let col = as_column(&isnull.expr)?;
            if !col.eq_ignore_ascii_case(self.column()) {
                return None;
            }
            return match self.tp {
                PartitionType::Range | PartitionType::List => {
                    self.definitions.first().map(|def| vec![def.id])
                }
                _ => self.locate(&Datum::Null).ok().map(|id| vec![id]),
            };
        }
        // `col IN (v1, v2, …)` prunes to the union of the per-value partitions.
        if let ExprNode::InList(list) = expr
            && !list.not
        {
            // Only the partition key column may drive pruning; an unrelated
            // column (`id IN (…)` under a different key) pins nothing.
            let col = as_column(&list.expr)?;
            if !col.eq_ignore_ascii_case(self.column()) {
                return None;
            }
            let mut ids = Vec::new();
            for item in &list.list {
                let value = const_datum(item)?;
                ids = union(&ids, &self.prune_point(&value));
            }
            return Some(ids);
        }
        let ExprNode::Binary(bin) = expr else {
            return None;
        };
        match bin.op {
            Op::LogicAnd => {
                if let Some(ids) = self.hash_conjunction_range(expr) {
                    return Some(ids);
                }
                let left = self.prune_expr(&bin.left);
                let right = self.prune_expr(&bin.right);
                match (left, right) {
                    (Some(l), Some(r)) => Some(intersect(&l, &r)),
                    (Some(l), None) => Some(l),
                    (None, Some(r)) => Some(r),
                    (None, None) => None,
                }
            }
            Op::LogicOr => {
                let left = self.prune_expr(&bin.left)?;
                let right = self.prune_expr(&bin.right)?;
                Some(union(&left, &right))
            }
            Op::Eq | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                self.prune_cmp(bin.op, &bin.left, &bin.right)
            }
            _ => None,
        }
    }

    /// TiDB `getUsedHashPartitions`: when a conjunctive integer range spans
    /// fewer values than there are partitions, enumerate every value in the
    /// range and collect the hash targets (`a >= 4 and a <= 5` → p0,p1).
    fn hash_conjunction_range(&self, expr: &ExprNode) -> Option<Vec<i64>> {
        if !matches!(self.tp, PartitionType::Hash | PartitionType::Key) {
            return None;
        }
        let mut low: Option<(i128, bool)> = None;
        let mut high: Option<(i128, bool)> = None;
        collect_cmp_bounds(expr, self.column(), &mut low, &mut high)?;
        let (lo, lo_inc) = low?;
        let (hi, hi_inc) = high?;
        let n = self.definitions.len() as i128;
        let pos_low = if lo_inc { lo } else { lo.saturating_add(1) };
        let pos_high = if hi_inc { hi } else { hi.saturating_sub(1) };
        if pos_high < pos_low {
            return Some(Vec::new());
        }
        let range_scalar = pos_high - pos_low;
        if range_scalar >= n {
            return None;
        }
        let mut out = Vec::new();
        for offset in 0..=range_scalar {
            let raw = pos_low + offset;
            let datum = if raw > i64::MAX as i128 {
                // UNSIGNED range above `i64::MAX` (`bigint unsigned` BETWEEN).
                Datum::U64(u64::try_from(raw).ok()?)
            } else {
                Datum::I64(raw as i64)
            };
            let Ok(id) = self.locate(&datum) else {
                return None;
            };
            if !out.contains(&id) {
                out.push(id);
            }
        }
        Some(out)
    }

    fn prune_cmp(&self, op: Op, left: &ExprNode, right: &ExprNode) -> Option<Vec<i64>> {
        let (col, value, flipped) = match (as_column(left), const_datum(right)) {
            (Some(col), Some(datum)) => (col, datum, false),
            _ => match (const_datum(left), as_column(right)) {
                (Some(datum), Some(col)) => (col, datum, true),
                _ => return None,
            },
        };
        if !col.eq_ignore_ascii_case(self.column()) {
            return None;
        }
        let op = if flipped { flip_cmp(op)? } else { op };
        match self.tp {
            PartitionType::Hash | PartitionType::Key | PartitionType::List => {
                if op != Op::Eq {
                    return None;
                }
                self.locate(&value).ok().map(|id| vec![id])
            }
            PartitionType::Range => Some(self.prune_range(op, &value)),
        }
    }

    /// Partitions that can hold `value` (hash/key/list point or range bound).
    fn prune_point(&self, value: &Datum) -> Vec<i64> {
        match self.tp {
            PartitionType::Range => self.prune_range(Op::Eq, value),
            _ => self.locate(value).map(|id| vec![id]).unwrap_or_default(),
        }
    }

    fn prune_range(&self, op: Op, value: &Datum) -> Vec<i64> {
        if matches!(value, Datum::Null) {
            return self
                .definitions
                .first()
                .map(|d| vec![d.id])
                .unwrap_or_default();
        }
        let mut out = Vec::new();
        let mut low: Option<&str> = None;
        for def in &self.definitions {
            let high = if def.is_maxvalue() {
                None
            } else {
                def.less_than.first().map(String::as_str)
            };
            if range_overlaps_bound(op, value, low, high) {
                out.push(def.id);
            }
            if high.is_some() {
                low = high;
            }
        }
        out
    }
}

impl TableInfo {
    pub fn physical_ids(&self) -> Vec<i64> {
        match &self.partition {
            Some(part) if part.enable && !part.definitions.is_empty() => part.physical_ids(),
            _ => vec![self.id],
        }
    }

    pub fn locate_partition(&self, value: &Datum) -> Result<i64> {
        match &self.partition {
            Some(part) if part.enable => part.locate(value),
            _ => Ok(self.id),
        }
    }

    /// RANGE COLUMNS uses every partition-key column; other types keep the first.
    pub fn locate_partition_row(&self, values: &[Datum]) -> Result<i64> {
        match &self.partition {
            Some(part) if part.enable && !part.columns.is_empty() => {
                part.locate_tuple(&self.partition_values(values))
            }
            _ => {
                let value = self.partition_value(values).cloned().unwrap_or(Datum::Null);
                self.locate_partition(&value)
            }
        }
    }

    pub fn partition_values(&self, values: &[Datum]) -> Vec<Datum> {
        let Some(part) = &self.partition else {
            return Vec::new();
        };
        part.columns
            .iter()
            .map(|name| {
                self.columns
                    .iter()
                    .position(|c| c.name.o().eq_ignore_ascii_case(name.o()))
                    .and_then(|idx| values.get(idx).cloned())
                    .unwrap_or(Datum::Null)
            })
            .collect()
    }

    pub fn prune_partitions(&self, selection: Option<&ExprNode>) -> Vec<i64> {
        match &self.partition {
            Some(part) if part.enable && !part.definitions.is_empty() => part.prune(selection),
            _ => vec![self.id],
        }
    }

    pub fn partition_value<'a>(&self, values: &'a [Datum]) -> Option<&'a Datum> {
        let part = self.partition.as_ref()?;
        let idx = self
            .columns
            .iter()
            .position(|c| c.name.o().eq_ignore_ascii_case(part.column()))?;
        values.get(idx)
    }
}

fn tuple_less(row: &[Datum], bound: &[String]) -> bool {
    for (i, b) in bound.iter().enumerate() {
        if b.eq_ignore_ascii_case("MAXVALUE") {
            return true;
        }
        let value = row.get(i).unwrap_or(&Datum::Null);
        match compare_bound(value, b) {
            Some(Ordering::Less) => return true,
            Some(Ordering::Greater) => return false,
            Some(Ordering::Equal) => continue,
            // Type-coercion gaps: keep the row in the current partition.
            None => return false,
        }
    }
    false
}

/// Compare a partition-key value against a `LESS THAN` bound. Bounds keep their
/// literal text, so DATETIME/VARCHAR RANGE COLUMNS compare by value (`2012-12-10`
/// vs `2022-12-30`), not by the integer parsing used before literal bounds.
fn compare_bound(value: &Datum, bound: &str) -> Option<Ordering> {
    match value {
        Datum::I64(_) | Datum::Year(_) | Datum::U64(_) => {
            let v = datum_i64(value)?;
            if let Ok(b) = bound.parse::<i64>() {
                return Some(v.cmp(&b));
            }
            let b = bound.parse::<f64>().ok()?;
            (v as f64).partial_cmp(&b)
        }
        Datum::Bit(b) => {
            let v = b.iter().fold(0u128, |acc, x| (acc << 8) | u128::from(*x));
            if let Ok(bound_i) = bound.parse::<u128>() {
                return Some(v.cmp(&bound_i));
            }
            (v as f64).partial_cmp(&bound.parse::<f64>().ok()?)
        }
        Datum::F64(v) => v.partial_cmp(&bound.parse::<f64>().ok()?),
        Datum::Bytes(v) => Some(v.as_slice().cmp(bound.as_bytes())),
        Datum::Time(t) => {
            let b = parse_bound_time(bound)?;
            Some(t.cmp(&b))
        }
        Datum::Decimal(d) => {
            let v: f64 = d.to_string().parse().ok()?;
            let b = bound.parse::<f64>().ok()?;
            v.partial_cmp(&b)
        }
        Datum::Null | Datum::Enum(_) => None,
    }
}

fn parse_bound_time(bound: &str) -> Option<sealdb_types::time::Time> {
    sealdb_types::time::parse_timestamp_literal(bound)
        .ok()
        .or_else(|| sealdb_types::time::parse_date_literal(bound).ok())
}

/// Partition interval is `[low, high)`; true when a value of the predicate's
/// type can satisfy `op` inside it.
fn range_overlaps_bound(op: Op, value: &Datum, low: Option<&str>, high: Option<&str>) -> bool {
    let low_cmp = low.and_then(|l| compare_bound(value, l));
    let high_cmp = high.and_then(|h| compare_bound(value, h));
    // No usable comparison (unsupported type): prune conservatively.
    if (low.is_some() && low_cmp.is_none()) || (high.is_some() && high_cmp.is_none()) {
        return true;
    }
    let above_low = low.is_none() || low_cmp.is_some_and(|c| c != Ordering::Less);
    match op {
        Op::Eq => above_low && high.is_none_or(|_| high_cmp == Some(Ordering::Less)),
        Op::Lt => low.is_none() || low_cmp == Some(Ordering::Greater),
        Op::Le => above_low,
        Op::Gt => match high {
            // Integer partitions are discrete: the largest value is `high - 1`.
            None => true,
            Some(h) => match (datum_i64(value), h.parse::<i64>()) {
                (Some(v), Ok(h)) => h.checked_sub(1).is_some_and(|m| m > v),
                _ => high_cmp == Some(Ordering::Less),
            },
        },
        Op::Ge => high.is_none() || high_cmp == Some(Ordering::Less),
        _ => true,
    }
}

fn flip_cmp(op: Op) -> Option<Op> {
    Some(match op {
        Op::Eq => Op::Eq,
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
        _ => return None,
    })
}

fn collect_cmp_bounds(
    expr: &ExprNode,
    column: &str,
    low: &mut Option<(i128, bool)>,
    high: &mut Option<(i128, bool)>,
) -> Option<()> {
    match expr {
        ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
            collect_cmp_bounds(&bin.left, column, low, high)?;
            collect_cmp_bounds(&bin.right, column, low, high)
        }
        ExprNode::Binary(bin) => {
            let (col, value, op) = match (as_column(&bin.left), const_datum(&bin.right)) {
                (Some(col), Some(datum)) => (col, datum, bin.op),
                _ => match (const_datum(&bin.left), as_column(&bin.right)) {
                    (Some(datum), Some(col)) => (col, datum, flip_cmp(bin.op)?),
                    _ => return None,
                },
            };
            if !col.eq_ignore_ascii_case(column) {
                return None;
            }
            let value = datum_i128(&value)?;
            match op {
                Op::Ge => {
                    if low.is_none_or(|(v, _)| value > v) {
                        *low = Some((value, true));
                    }
                }
                Op::Gt => {
                    if low.is_none_or(|(v, _)| value >= v) {
                        *low = Some((value, false));
                    }
                }
                Op::Le => {
                    if high.is_none_or(|(v, _)| value < v) {
                        *high = Some((value, true));
                    }
                }
                Op::Lt => {
                    if high.is_none_or(|(v, _)| value <= v) {
                        *high = Some((value, false));
                    }
                }
                _ => return None,
            }
            Some(())
        }
        _ => None,
    }
}

fn intersect(left: &[i64], right: &[i64]) -> Vec<i64> {
    left.iter()
        .copied()
        .filter(|id| right.contains(id))
        .collect()
}

fn union(left: &[i64], right: &[i64]) -> Vec<i64> {
    let mut out = left.to_vec();
    for id in right {
        if !out.contains(id) {
            out.push(*id);
        }
    }
    out
}

fn datum_i128(value: &Datum) -> Option<i128> {
    match value {
        Datum::I64(v) => Some(*v as i128),
        Datum::U64(v) => Some(*v as i128),
        Datum::F64(v) if v.fract() == 0.0 => Some(*v as i128),
        _ => None,
    }
}

fn datum_i64(value: &Datum) -> Option<i64> {
    match value {
        Datum::I64(v) => Some(*v),
        Datum::Year(v) => Some(*v),
        Datum::U64(v) => i64::try_from(*v).ok(),
        _ => None,
    }
}

fn as_column(expr: &ExprNode) -> Option<&str> {
    match expr {
        ExprNode::Column(col) => Some(col.name.as_str()),
        _ => None,
    }
}

/// Fold `Value` / `UnaryMinus(Value)` into a Datum for partition prune.
fn const_datum(expr: &ExprNode) -> Option<Datum> {
    match expr {
        ExprNode::Value(ValueExpr { datum }) => Some(datum.clone()),
        ExprNode::Unary(u) if u.op == Op::UnaryMinus => match u.expr.as_ref() {
            ExprNode::Value(ValueExpr {
                datum: Datum::I64(v),
            }) => Some(Datum::I64(v.wrapping_neg())),
            ExprNode::Value(ValueExpr {
                datum: Datum::U64(v),
            }) => Some(Datum::I64((*v as i64).wrapping_neg())),
            ExprNode::Value(ValueExpr {
                datum: Datum::F64(v),
            }) => Some(Datum::F64(-*v)),
            _ => None,
        },
        _ => None,
    }
}

fn no_partition_value(value: &Datum) -> Error {
    Error::new(
        ERR_NO_PARTITION_FOR_GIVEN_VALUE,
        format!("Table has no partition for value {}", value.display()),
    )
}

fn no_partition() -> Error {
    Error::new(
        ERR_NO_PARTITION_FOR_GIVEN_VALUE,
        "Table has no partition for value from column_list",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::column::ColumnInfo;
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, ValueExpr};
    use sealdb_types::FieldType;

    fn range_table() -> TableInfo {
        let mut info = TableInfo::new(
            100,
            "t",
            vec![ColumnInfo::new(1, 0, "id", FieldType::new_longlong())],
        );
        info.partition = Some(PartitionInfo::new(
            PartitionType::Range,
            "id",
            vec![
                PartitionDefinition::new(101, "p0", vec!["10".into()]),
                PartitionDefinition::new(102, "p1", vec!["20".into()]),
                PartitionDefinition::new(103, "p2", vec!["MAXVALUE".into()]),
            ],
        ));
        info
    }

    fn pred(op: Op, v: i64) -> ExprNode {
        ExprNode::Binary(BinaryOpExpr {
            op,
            left: Box::new(ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: None,
                name: "id".into(),
            })),
            right: Box::new(ExprNode::Value(ValueExpr {
                datum: Datum::I64(v),
            })),
            single_cnf_item: false,
        })
    }

    #[test]
    fn test_range_prune_skips_maxvalue_on_lt() {
        let info = range_table();
        let lt = pred(Op::Lt, 10);
        let le = pred(Op::Le, 10);
        let eq = pred(Op::Eq, 15);
        let ge = pred(Op::Ge, 20);
        assert_eq!(info.prune_partitions(Some(&lt)), vec![101]);
        assert_eq!(info.prune_partitions(Some(&le)), vec![101, 102]);
        assert_eq!(info.prune_partitions(Some(&eq)), vec![102]);
        assert_eq!(info.prune_partitions(Some(&ge)), vec![103]);
    }
}
