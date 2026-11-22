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

/// Scalar inside a LIST / LIST COLUMNS value row.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum ListLiteral {
    Null,
    Number(String),
    String(String),
}

/// One `VALUES IN` entry (`DEFAULT` or a value row).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum ListInEntry {
    Default,
    Row(Vec<ListLiteral>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PartitionDefinition {
    pub id: i64,
    pub name: CIStr,
    /// RANGE bound as a decimal string, or `MAXVALUE`. Empty for HASH/KEY.
    #[serde(rename = "less_than", default)]
    pub less_than: Vec<String>,
    /// LIST / LIST COLUMNS `VALUES IN` entries. Empty for RANGE/HASH/KEY.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_values: Vec<ListInEntry>,
    /// `PARTITION p DEFAULT` shorthand (SHOW CREATE prints bare `DEFAULT`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub list_default: bool,
    /// TiDB placement attributes (`ALTER TABLE … PARTITION p ATTRIBUTES`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<(String, String)>,
    /// Optional partition comment (`COMMENT '…'`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub comment: String,
    /// Optional `PLACEMENT POLICY=` name for this partition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_policy: Option<String>,
}

impl PartitionDefinition {
    pub fn new(id: i64, name: impl Into<String>, less_than: Vec<String>) -> Self {
        Self {
            id,
            name: CIStr::new(name),
            less_than,
            in_values: Vec::new(),
            list_default: false,
            attributes: Vec::new(),
            comment: String::new(),
            placement_policy: None,
        }
    }

    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = comment.into();
        self
    }

    pub fn with_placement_policy(mut self, policy: Option<String>) -> Self {
        self.placement_policy = policy;
        self
    }

    pub fn with_in_values(mut self, values: Vec<ListInEntry>) -> Self {
        self.in_values = values;
        self
    }

    pub fn with_list_default(mut self, list_default: bool) -> Self {
        self.list_default = list_default;
        self
    }

    pub fn has_list_default(&self) -> bool {
        self.in_values
            .iter()
            .any(|e| matches!(e, ListInEntry::Default))
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
    /// HASH/KEY partitions declared with `PARTITIONS n` (SHOW CREATE prints the
    /// `PARTITIONS n` shorthand instead of the partition definitions).
    #[serde(default)]
    pub uniform: bool,
    /// `RANGE COLUMNS(...)` / `LIST COLUMNS(...)` form (SHOW CREATE prints
    /// `RANGE COLUMNS(...)` instead of `RANGE (...)`).
    #[serde(default)]
    pub range_columns: bool,
    /// Partition-key column uses a case-insensitive collation
    /// (`utf8mb4_general_ci`, …): range bounds, LIST literals, and pruning
    /// compare strings through the collation sort key, not raw bytes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collation_ci: bool,
    /// TiDB RANGE INTERVAL step text (empty when not an INTERVAL table).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub interval_expr: String,
    /// INTERVAL table was created with MAXVALUE PARTITION.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interval_maxvalue: bool,
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
            uniform: false,
            range_columns: false,
            collation_ci: false,
            interval_expr: String::new(),
            interval_maxvalue: false,
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
            if compare_bound(value, bound, self.collation_ci) == Some(Ordering::Less) {
                return Ok(def.id);
            }
        }
        if self.range_columns {
            Err(no_partition())
        } else {
            Err(no_partition_value(value))
        }
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

    /// TiDB `KEY` partitioning: CRC-32/IEEE over the collation sort key of the
    /// value's text form, then `% partitions`. Distinct from HASH (modulo of
    /// the raw value), so `9223372036854775807` and `9223372036854775808`
    /// both land in `p2` of five.
    fn locate_key(&self, value: &Datum) -> Result<i64> {
        if self.definitions.is_empty() {
            return Err(no_partition());
        }
        let idx =
            super::key_hash::key_partition_index(value, self.definitions.len(), self.collation_ci)
                .ok_or_else(no_partition)?;
        Ok(self.definitions[idx].id)
    }

    /// Multi-column `PARTITION BY KEY (c1, c2, …)`: CRC-32 over the
    /// concatenated per-column keys (same layout as single-column KEY).
    fn locate_key_columns(&self, values: &[Datum]) -> Result<i64> {
        if self.definitions.is_empty() {
            return Err(no_partition());
        }
        let idx = super::key_hash::key_partition_index_multi(
            values,
            self.definitions.len(),
            self.collation_ci,
        )
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
        // LIST COLUMNS rows match the declared literal rows (a `DEFAULT`
        // partition catches the rest); they are not ordered range bounds.
        if self.tp == PartitionType::List {
            return self.locate_list_row(values);
        }
        for def in &self.definitions {
            if tuple_less(values, &def.less_than, self.collation_ci) {
                return Ok(def.id);
            }
        }
        Err(no_partition())
    }

    fn locate_list(&self, value: &Datum) -> Result<i64> {
        // LIST partitions accept only the declared values; `NULL` matches an
        // explicit `NULL` list entry (TiDB `VALUES IN (…, NULL)`). Unmatched
        // values fall through to a `DEFAULT` partition when present.
        let row = match list_row_from_datum(value) {
            Some(row) => row,
            None => return Err(no_partition_value(value)),
        };
        if let Some(def) = self.definitions.iter().find(|def| {
            def.in_values
                .iter()
                .any(|e| matches!(e, ListInEntry::Row(r) if list_row_matches(r, &row, self.collation_ci)))
        }) {
            return Ok(def.id);
        }
        if let Some(def) = self.definitions.iter().find(|def| def.has_list_default()) {
            return Ok(def.id);
        }
        Err(no_partition_value(value))
    }

    /// Locate a LIST COLUMNS row (one Datum per key column).
    pub fn locate_list_row(&self, values: &[Datum]) -> Result<i64> {
        let row: Vec<ListLiteral> = values
            .iter()
            .map(|v| list_literal_from_datum(v).ok_or_else(no_partition))
            .collect::<Result<Vec<_>>>()?;
        if let Some(def) = self.definitions.iter().find(|def| {
            def.in_values
                .iter()
                .any(|e| matches!(e, ListInEntry::Row(r) if list_row_matches(r, &row, self.collation_ci)))
        }) {
            return Ok(def.id);
        }
        if let Some(def) = self.definitions.iter().find(|def| def.has_list_default()) {
            return Ok(def.id);
        }
        Err(no_partition())
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
        // `col IS NULL`: TiDB routes NULL to the declared NULL entry of a LIST
        // partition, otherwise to the first partition (range NULL sorts lowest;
        // hash(NULL) is 0).
        if let ExprNode::IsNull(isnull) = expr {
            if isnull.not {
                return None;
            }
            let col = as_column(&isnull.expr)?;
            if !col.eq_ignore_ascii_case(self.column()) {
                return None;
            }
            return self.locate(&Datum::Null).ok().map(|id| vec![id]);
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
                if matches!(self.tp, PartitionType::List)
                    && let Some(ids) = self.prune_list_conjunction_range(expr)
                {
                    return Some(ids);
                }
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
        // `COLLATE` on the literal changes the comparison collation. TiDB can
        // only keep pruning equality when the value can be located in the
        // partition bounds: a `*_ci` literal is order-compatible with `*_ci`
        // bounds, and every literal is compatible with `*_bin` bounds, but a
        // `*_ci` literal cannot be binary-searched over binary bounds.
        let (left, left_ci) = unwrap_explicit_collate(left);
        let (right, right_ci) = unwrap_explicit_collate(right);
        if let Some(explicit_ci) = left_ci.or(right_ci) {
            if op != Op::Eq || (explicit_ci && !self.collation_ci) {
                return None;
            }
        }
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
            PartitionType::Hash | PartitionType::Key => {
                if op != Op::Eq {
                    return None;
                }
                self.locate(&value).ok().map(|id| vec![id])
            }
            PartitionType::List => {
                if op == Op::Eq {
                    self.locate(&value).ok().map(|id| vec![id])
                } else {
                    Some(self.prune_list_range(op, &value))
                }
            }
            PartitionType::Range => Some(self.prune_range(op, &value)),
        }
    }

    /// LIST pruning for a range predicate: a partition is kept when one of its
    /// declared values satisfies the predicate. The `DEFAULT` partition always
    /// stays because it can hold any unmatched value.
    fn prune_list_range(&self, op: Op, value: &Datum) -> Vec<i64> {
        let mut out = Vec::new();
        for def in &self.definitions {
            if def.has_list_default() {
                out.push(def.id);
                continue;
            }
            let hit = def.in_values.iter().any(|entry| match entry {
                ListInEntry::Row(row) if row.len() == 1 => {
                    list_literal_matches_op(&row[0], op, value, self.collation_ci)
                }
                _ => false,
            });
            if hit {
                out.push(def.id);
            }
        }
        out
    }

    /// LIST pruning for a conjunctive range (`a >= "3" and a <= "4"`).
    ///
    /// Testing each conjunct separately over-approximates: p2 holding `"2"`
    /// and `"7"` survives both `>= "3"` (via `"7"`) and `<= "4"` (via `"2"`)
    /// although no single declared value lies in the range. TiDB tests every
    /// value against the whole range, so fold the bounds first.
    fn prune_list_conjunction_range(&self, expr: &ExprNode) -> Option<Vec<i64>> {
        let mut low: Option<(Datum, bool)> = None;
        let mut high: Option<(Datum, bool)> = None;
        collect_list_bounds(expr, self.column(), &mut low, &mut high, self.collation_ci)?;
        let (low, low_inc) = low?;
        let (high, high_inc) = high?;
        let mut out = Vec::new();
        for def in &self.definitions {
            if def.has_list_default() {
                out.push(def.id);
                continue;
            }
            let hit = def.in_values.iter().any(|entry| match entry {
                ListInEntry::Row(row) if row.len() == 1 => {
                    let lo = if low_inc { Op::Ge } else { Op::Gt };
                    let hi = if high_inc { Op::Le } else { Op::Lt };
                    list_literal_matches_op(&row[0], lo, &low, self.collation_ci)
                        && list_literal_matches_op(&row[0], hi, &high, self.collation_ci)
                }
                _ => false,
            });
            if hit {
                out.push(def.id);
            }
        }
        Some(out)
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
            if range_overlaps_bound(op, value, low, high, self.collation_ci) {
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

    /// RANGE/LIST COLUMNS use every key column; HASH/KEY use locate (multi-col KEY hashes).
    pub fn locate_partition_row(&self, values: &[Datum]) -> Result<i64> {
        match &self.partition {
            Some(part)
                if part.enable && matches!(part.tp, PartitionType::Hash | PartitionType::Key) =>
            {
                if part.columns.len() > 1 {
                    return part.locate_key_columns(&self.partition_values(values));
                }
                let value = self.partition_value(values).cloned().unwrap_or(Datum::Null);
                part.locate(&value)
            }
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

fn tuple_less(row: &[Datum], bound: &[String], ci: bool) -> bool {
    for (i, b) in bound.iter().enumerate() {
        if b.eq_ignore_ascii_case("MAXVALUE") {
            return true;
        }
        let value = row.get(i).unwrap_or(&Datum::Null);
        match compare_bound(value, b, ci) {
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
fn strip_bound_quotes(bound: &str) -> &str {
    let bytes = bound.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"'))
    {
        &bound[1..bound.len() - 1]
    } else {
        bound
    }
}

/// Case-insensitive collation sort key for `utf8mb4_general_ci`-style
/// comparisons: lowercases and folds the Latin-1/Latin Extended accents TiDB's
/// UCA table treats as the base letter (`ä` sorts as `a`).
pub(crate) fn ci_sort_key(text: &str) -> Vec<char> {
    sealdb_types::ci_fold_str(text).chars().collect()
}

/// Compare two strings under the partition key's collation.
fn compare_strings(a: &str, b: &str, ci: bool) -> Ordering {
    if ci {
        ci_sort_key(a).cmp(&ci_sort_key(b))
    } else {
        a.as_bytes().cmp(b.as_bytes())
    }
}

/// Compare two partition bound / LIST literal strings the way `locate` does
/// (public for DDL bound-order validation).
pub fn compare_partition_strings(a: &str, b: &str, ci: bool) -> Ordering {
    compare_strings(a, b, ci)
}

fn compare_bound(value: &Datum, bound: &str, ci: bool) -> Option<Ordering> {
    let raw = strip_bound_quotes(bound);
    let unescaped = unescape_mysql_string(raw);
    let bound = unescaped.as_str();
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
        Datum::Bytes(v) => Some(compare_strings(&String::from_utf8_lossy(v), bound, ci)),
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
    let bound = strip_bound_quotes(bound);
    sealdb_types::time::parse_timestamp_literal(bound)
        .ok()
        .or_else(|| sealdb_types::time::parse_date_literal(bound).ok())
}

/// Partition interval is `[low, high)`; true when a value of the predicate's
/// type can satisfy `op` inside it.
fn range_overlaps_bound(
    op: Op,
    value: &Datum,
    low: Option<&str>,
    high: Option<&str>,
    ci: bool,
) -> bool {
    let low_cmp = low.and_then(|l| compare_bound(value, l, ci));
    let high_cmp = high.and_then(|h| compare_bound(value, h, ci));
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

/// Fold `col >= lo AND col <= hi` conjuncts into `Datum` bounds for LIST
/// pruning. Unlike [`collect_cmp_bounds`] the operands need not be numeric:
/// LIST COLUMNS varchar / date keys compare as text.
fn collect_list_bounds(
    expr: &ExprNode,
    column: &str,
    low: &mut Option<(Datum, bool)>,
    high: &mut Option<(Datum, bool)>,
    ci: bool,
) -> Option<()> {
    match expr {
        ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
            collect_list_bounds(&bin.left, column, low, high, ci)?;
            collect_list_bounds(&bin.right, column, low, high, ci)
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
            match op {
                Op::Ge => set_lower(low, value, true, ci),
                Op::Gt => set_lower(low, value, false, ci),
                Op::Le => set_upper(high, value, true, ci),
                Op::Lt => set_upper(high, value, false, ci),
                _ => return None,
            }
            Some(())
        }
        _ => None,
    }
}

fn set_lower(slot: &mut Option<(Datum, bool)>, value: Datum, inclusive: bool, ci: bool) {
    let tighter = match slot {
        None => true,
        Some((current, _)) => {
            matches!(datum_ordering(&value, current, ci), Some(Ordering::Greater))
        }
    };
    if tighter {
        *slot = Some((value, inclusive));
    } else if let Some((current, current_inc)) = slot.as_mut()
        && datum_ordering(&value, current, ci) == Some(Ordering::Equal)
    {
        *current_inc = *current_inc && inclusive;
    }
}

fn set_upper(slot: &mut Option<(Datum, bool)>, value: Datum, inclusive: bool, ci: bool) {
    let tighter = match slot {
        None => true,
        Some((current, _)) => {
            matches!(datum_ordering(&value, current, ci), Some(Ordering::Less))
        }
    };
    if tighter {
        *slot = Some((value, inclusive));
    } else if let Some((current, current_inc)) = slot.as_mut()
        && datum_ordering(&value, current, ci) == Some(Ordering::Equal)
    {
        *current_inc = *current_inc && inclusive;
    }
}

fn datum_ordering(a: &Datum, b: &Datum, ci: bool) -> Option<Ordering> {
    match (datum_i128(a), datum_i128(b)) {
        (Some(l), Some(r)) => Some(l.cmp(&r)),
        _ => {
            let a = String::from_utf8_lossy(&a.as_bytes()).into_owned();
            let b = String::from_utf8_lossy(&b.as_bytes()).into_owned();
            Some(compare_strings(&a, &b, ci))
        }
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

fn list_literal_from_datum(value: &Datum) -> Option<ListLiteral> {
    match value {
        Datum::Null => Some(ListLiteral::Null),
        Datum::I64(v) => Some(ListLiteral::Number(v.to_string())),
        Datum::Year(v) => Some(ListLiteral::Number(v.to_string())),
        Datum::U64(v) => Some(ListLiteral::Number(v.to_string())),
        Datum::Bytes(b) => Some(ListLiteral::String(String::from_utf8_lossy(b).into_owned())),
        Datum::Decimal(d) => Some(ListLiteral::Number(d.to_string())),
        Datum::Time(t) => Some(ListLiteral::String(t.to_string())),
        _ => None,
    }
}

fn list_row_from_datum(value: &Datum) -> Option<Vec<ListLiteral>> {
    Some(vec![list_literal_from_datum(value)?])
}

fn list_row_matches(declared: &[ListLiteral], got: &[ListLiteral], ci: bool) -> bool {
    if declared.len() != got.len() {
        return false;
    }
    declared
        .iter()
        .zip(got.iter())
        .all(|(a, b)| list_literal_eq(a, b, ci))
}

fn list_literal_eq(a: &ListLiteral, b: &ListLiteral, ci: bool) -> bool {
    match (a, b) {
        (ListLiteral::Null, ListLiteral::Null) => true,
        (ListLiteral::Number(x), ListLiteral::Number(y)) => {
            // Compare numerically when both parse, else textually.
            match (x.parse::<i128>(), y.parse::<i128>()) {
                (Ok(xv), Ok(yv)) => xv == yv,
                _ => x == y,
            }
        }
        (ListLiteral::String(x), ListLiteral::String(y)) => {
            compare_strings(x, y, ci) == Ordering::Equal || date_midnight_eq(x, y)
        }
        // LIST COLUMNS varchar often stores coerced numbers as strings.
        (ListLiteral::Number(n), ListLiteral::String(s))
        | (ListLiteral::String(s), ListLiteral::Number(n)) => {
            compare_strings(n, s, ci) == Ordering::Equal
        }
        _ => false,
    }
}

/// A `DATE`-typed declared literal (`'2023-08-29'`) matches the midnight
/// `DATETIME` row value TiDB stores for it (`'2023-08-29 00:00:00'`).
fn date_midnight_eq(a: &str, b: &str) -> bool {
    let (date, datetime) = if a.len() == 10 && b.len() >= 19 {
        (a, b)
    } else if b.len() == 10 && a.len() >= 19 {
        (b, a)
    } else {
        return false;
    };
    datetime.starts_with(date) && datetime[10..].starts_with(" 00:00:00")
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
/// Strip an `expr COLLATE name` wrapper, returning the inner expression and
/// whether the collation is case-insensitive (`*_ci`).
fn unwrap_explicit_collate(expr: &ExprNode) -> (&ExprNode, Option<bool>) {
    let ExprNode::Func(func) = expr else {
        return (expr, None);
    };
    if !func.name.eq_ignore_ascii_case("__collate") {
        return (expr, None);
    }
    let Some(inner) = func.args.first() else {
        return (expr, None);
    };
    let ci = match func.args.get(1) {
        Some(ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(name),
        })) => String::from_utf8_lossy(name)
            .to_ascii_lowercase()
            .ends_with("_ci"),
        _ => false,
    };
    (inner, Some(ci))
}

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

/// Whether a declared LIST value satisfies `literal <op> value`.
fn list_literal_matches_op(literal: &ListLiteral, op: Op, value: &Datum, ci: bool) -> bool {
    // Declared numbers compare numerically; quoted values (LIST COLUMNS
    // varchar / date keys) compare as text even when they look numeric, so
    // `a <= "4"` keeps a partition holding `"2"` but not `"10"`.
    let literal_num = match literal {
        ListLiteral::Number(n) => n.parse::<i128>().ok(),
        _ => None,
    };
    let value_num = datum_i128(value);
    let ordering = match (literal_num, value_num) {
        (Some(l), Some(r)) => l.cmp(&r),
        _ => {
            let l = match literal {
                ListLiteral::Number(n) => n.clone(),
                ListLiteral::String(s) => s.clone(),
                ListLiteral::Null => return false,
            };
            let r = String::from_utf8_lossy(&value.as_bytes()).into_owned();
            compare_strings(&l, &r, ci)
        }
    };
    match op {
        Op::Eq => ordering == Ordering::Equal,
        Op::Lt => ordering == Ordering::Less,
        Op::Le => ordering != Ordering::Greater,
        Op::Gt => ordering == Ordering::Greater,
        Op::Ge => ordering != Ordering::Less,
        _ => true,
    }
}

/// Escape a value for MySQL string-literal output: a backslash, quote and the
/// control characters TiDB spells with an escape sequence.
pub fn escape_mysql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("''"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            '\x1a' => out.push_str("\\Z"),
            other => out.push(other),
        }
    }
    out
}

/// Reverse of [`escape_mysql_string`] for the inner text of a quoted literal.
pub fn unescape_mysql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\'' {
            // A doubled quote stays one quote.
            if chars.clone().next() == Some('\'') {
                chars.next();
            }
            out.push('\'');
            continue;
        }
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some('b') => out.push('\x08'),
            Some('Z') => out.push('\x1a'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('"') => out.push('"'),
            Some('%') => out.push_str("\\%"),
            Some('_') => out.push_str("\\_"),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
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
