use sqlparser::dialect::MySqlDialect;

/// TiDB-facing dialect. sqlparser's MySQL dialect is the starting point;
/// extra TiDB syntax is handled by preprocess + lowering.
pub type TiDBDialect = MySqlDialect;

pub fn dialect() -> TiDBDialect {
    MySqlDialect {}
}
