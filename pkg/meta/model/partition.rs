use super::table::TableInfo;
use crate::model::job::{STATE_PUBLIC, SchemaState};
use sealdb_errno::{ERR_NO_PARTITION_FOR_GIVEN_VALUE, Error, Result};
use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
use sealdb_parser::model::CIStr;
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;
use serde::{Deserialize, Serialize};

/// TiDB `model.PartitionType` core cases for F19.
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
    /// LIST `VALUES IN` integers. Empty for RANGE/HASH/KEY.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_values: Vec<i64>,
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

    pub fn with_in_values(mut self, values: Vec<i64>) -> Self {
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

/// TiDB `model.PartitionInfo` core fields used by F19.
/// Completeness wave: RANGE COLUMNS, subpartition, reorg states.
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
            PartitionType::Hash | PartitionType::Key => self.locate_hash(value),
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
        let Some(v) = datum_i64(value) else {
            return Err(no_partition());
        };
        for def in &self.definitions {
            if def.is_maxvalue() {
                return Ok(def.id);
            }
            if let Some(bound) = def.bound()
                && v < bound
            {
                return Ok(def.id);
            }
        }
        Err(no_partition())
    }

    fn locate_hash(&self, value: &Datum) -> Result<i64> {
        if self.definitions.is_empty() {
            return Err(no_partition());
        }
        if matches!(value, Datum::Null) {
            return Ok(self.definitions[0].id);
        }
        let Some(v) = datum_i64(value) else {
            return Err(no_partition());
        };
        let n = self.definitions.len() as i64;
        let idx = ((v % n) + n) % n;
        Ok(self.definitions[idx as usize].id)
    }

    /// Lexicographic RANGE COLUMNS compare: the first bound the row is strictly
    /// less than (MAXVALUE is +inf) is the partition.
    pub fn locate_tuple(&self, values: &[Datum]) -> Result<i64> {
        if self.columns.len() <= 1 {
            return self.locate(values.first().unwrap_or(&Datum::Null));
        }
        for def in &self.definitions {
            if tuple_less(values, &def.less_than) {
                return Ok(def.id);
            }
        }
        Err(no_partition())
    }

    fn locate_list(&self, value: &Datum) -> Result<i64> {
        if matches!(value, Datum::Null) {
            return self
                .definitions
                .first()
                .map(|d| d.id)
                .ok_or_else(no_partition);
        }
        let Some(v) = datum_i64(value) else {
            return Err(no_partition());
        };
        self.definitions
            .iter()
            .find(|def| def.in_values.contains(&v))
            .map(|def| def.id)
            .ok_or_else(no_partition)
    }

    pub fn prune(&self, selection: Option<&ExprNode>) -> Vec<i64> {
        match selection {
            None => self.physical_ids(),
            Some(expr) => self.prune_expr(expr).unwrap_or_else(|| self.physical_ids()),
        }
    }

    fn prune_expr(&self, expr: &ExprNode) -> Option<Vec<i64>> {
        let ExprNode::Binary(bin) = expr else {
            return None;
        };
        match bin.op {
            Op::LogicAnd => {
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

    fn prune_cmp(&self, op: Op, left: &ExprNode, right: &ExprNode) -> Option<Vec<i64>> {
        let (col, value, flipped) = match (left, right) {
            (ExprNode::Column(col), ExprNode::Value(ValueExpr { datum })) => {
                (col.name.as_str(), datum, false)
            }
            (ExprNode::Value(ValueExpr { datum }), ExprNode::Column(col)) => {
                (col.name.as_str(), datum, true)
            }
            _ => return None,
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
                self.locate(value).ok().map(|id| vec![id])
            }
            PartitionType::Range => Some(self.prune_range(op, value)),
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
        let Some(v) = datum_i64(value) else {
            return self.physical_ids();
        };
        let mut out = Vec::new();
        let mut low = i64::MIN;
        for def in &self.definitions {
            let high = if def.is_maxvalue() { None } else { def.bound() };
            if range_overlaps(op, v, low, high) {
                out.push(def.id);
            }
            if let Some(bound) = high {
                low = bound;
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
            Some(part) if part.enable && part.columns.len() > 1 => {
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
        let rv = row.get(i).and_then(datum_i64).unwrap_or(i64::MIN);
        let Ok(bv) = b.parse::<i64>() else {
            return false;
        };
        if rv < bv {
            return true;
        }
        if rv > bv {
            return false;
        }
    }
    false
}

fn range_overlaps(op: Op, v: i64, low: i64, high: Option<i64>) -> bool {
    // Partition interval is [low, high).
    match op {
        Op::Eq => (v >= low) && high.is_none_or(|h| v < h),
        Op::Lt => low < v,
        Op::Le => low <= v,
        Op::Gt => high.is_none() || high.is_some_and(|h| h - 1 > v),
        Op::Ge => high.is_none() || high.is_some_and(|h| h > v),
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

fn datum_i64(value: &Datum) -> Option<i64> {
    match value {
        Datum::I64(v) => Some(*v),
        Datum::U64(v) => i64::try_from(*v).ok(),
        _ => None,
    }
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
