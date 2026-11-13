//! Builtin function name registry (TiDB `pkg/expression/builtin.go` `funcs` map).
//!
//! SealDB evaluates via [`crate::evaluator`]; this module is the 1:1 catalog
//! surface for TDD parity — expand as integrationtest suites demand.

/// Categories mirrored from TiDB's `funcs` registration blocks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FuncCategory {
    Common,
    Math,
    String,
    Control,
    Compare,
    Encryption,
    Json,
    Time,
    Info,
    Miscellaneous,
}

/// One builtin entry. `vectorized` is aspirational until VecEval lands.
#[derive(Clone, Debug)]
pub struct FuncMeta {
    pub name: &'static str,
    pub category: FuncCategory,
    pub min_args: i8,
    pub max_args: i8,
    pub implemented: bool,
    pub vectorized: bool,
}

/// TiDB-shaped catalog. `implemented` tracks SealDB scalar eval support today.
pub fn builtin_list() -> &'static [FuncMeta] {
    &FUNCS
}

const FUNCS: &[FuncMeta] = &[
    // common
    meta("COALESCE", FuncCategory::Common, 1, -1, true),
    meta("ISNULL", FuncCategory::Common, 1, 1, true),
    meta("GREATEST", FuncCategory::Common, 2, -1, true),
    meta("LEAST", FuncCategory::Common, 2, -1, true),
    meta("INTERVAL", FuncCategory::Common, 2, -1, false),
    // control
    meta("IF", FuncCategory::Control, 3, 3, true),
    meta("IFNULL", FuncCategory::Control, 2, 2, true),
    meta("NULLIF", FuncCategory::Control, 2, 2, false),
    meta("CASE", FuncCategory::Control, 2, -1, false),
    // math (subset)
    meta("ABS", FuncCategory::Math, 1, 1, true),
    meta("CEIL", FuncCategory::Math, 1, 1, false),
    meta("CEILING", FuncCategory::Math, 1, 1, false),
    meta("FLOOR", FuncCategory::Math, 1, 1, false),
    meta("ROUND", FuncCategory::Math, 1, 2, false),
    meta("SQRT", FuncCategory::Math, 1, 1, false),
    meta("POW", FuncCategory::Math, 2, 2, false),
    meta("POWER", FuncCategory::Math, 2, 2, false),
    meta("MOD", FuncCategory::Math, 2, 2, false),
    meta("PI", FuncCategory::Math, 0, 0, false),
    meta("RAND", FuncCategory::Math, 0, 1, false),
    // compare / logic helpers often seen as funcs
    meta("ISTRUE", FuncCategory::Compare, 1, 1, false),
    meta("ISFALSE", FuncCategory::Compare, 1, 1, false),
    // string (subset)
    meta("LOWER", FuncCategory::String, 1, 1, true),
    meta("LCASE", FuncCategory::String, 1, 1, true),
    meta("UPPER", FuncCategory::String, 1, 1, true),
    meta("UCASE", FuncCategory::String, 1, 1, true),
    meta("LENGTH", FuncCategory::String, 1, 1, true),
    meta("CHAR_LENGTH", FuncCategory::String, 1, 1, true),
    meta("CHARACTER_LENGTH", FuncCategory::String, 1, 1, true),
    meta("REVERSE", FuncCategory::String, 1, 1, true),
    meta("TRIM", FuncCategory::String, 1, 1, true),
    meta("LTRIM", FuncCategory::String, 1, 1, true),
    meta("RTRIM", FuncCategory::String, 1, 1, true),
    meta("SUBSTRING", FuncCategory::String, 2, 3, true),
    meta("SUBSTR", FuncCategory::String, 2, 3, true),
    meta("CONCAT", FuncCategory::String, 1, -1, true),
    meta("CONCAT_WS", FuncCategory::String, 2, -1, true),
    meta("REPLACE", FuncCategory::String, 3, 3, true),
    meta("LEFT", FuncCategory::String, 2, 2, true),
    meta("RIGHT", FuncCategory::String, 2, 2, true),
    meta("LPAD", FuncCategory::String, 3, 3, true),
    meta("RPAD", FuncCategory::String, 3, 3, true),
];

const fn meta(
    name: &'static str,
    category: FuncCategory,
    min_args: i8,
    max_args: i8,
    implemented: bool,
) -> FuncMeta {
    FuncMeta {
        name,
        category,
        min_args,
        max_args,
        implemented,
        vectorized: false,
    }
}

pub fn lookup(name: &str) -> Option<&'static FuncMeta> {
    let upper = name.to_ascii_uppercase();
    FUNCS.iter().find(|f| f.name == upper)
}

pub fn implemented_names() -> Vec<&'static str> {
    FUNCS
        .iter()
        .filter(|f| f.implemented)
        .map(|f| f.name)
        .collect()
}

/// TiDB `expression.Check`: invoking a native function with a parameter count
/// outside its declared `[min_args, max_args]` fails with 1582
/// (`ErrWrongParamcountToNativeFct`). Returns the accepted bounds on mismatch.
pub fn check_arg_count(name: &str, argc: usize) -> Result<(), (i8, i8)> {
    let Some(meta) = lookup(name) else {
        return Ok(());
    };
    let too_few = argc < meta.min_args as usize;
    let too_many = meta.max_args >= 0 && argc > meta.max_args as usize;
    if too_few || too_many {
        return Err((meta.min_args, meta.max_args));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_core_ported_funcs() {
        for n in ["IF", "IFNULL", "ABS", "COALESCE", "ISNULL"] {
            let m = lookup(n).unwrap_or_else(|| panic!("missing {n}"));
            assert!(m.implemented, "{n}");
        }
        assert!(builtin_list().len() >= 20);
    }
}
