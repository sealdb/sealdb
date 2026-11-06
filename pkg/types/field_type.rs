#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvalType {
    Int,
    Real,
    Decimal,
    String,
    Datetime,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FieldType {
    #[serde(rename = "Tp")]
    pub tp: u8,
    #[serde(rename = "Flag")]
    pub flag: u32,
    #[serde(rename = "Flen")]
    pub flen: i32,
    #[serde(rename = "Decimal")]
    pub decimal: i32,
    #[serde(rename = "Charset")]
    pub charset: String,
    #[serde(rename = "Collate")]
    pub collate: String,
    #[serde(skip)]
    pub unsigned: bool,
}

impl Default for FieldType {
    fn default() -> Self {
        Self {
            tp: TYPE_LONG_LONG,
            flag: 0,
            flen: -1,
            decimal: -1,
            charset: "utf8mb4".to_string(),
            collate: "utf8mb4_bin".to_string(),
            unsigned: false,
        }
    }
}

impl FieldType {
    pub fn new_longlong() -> Self {
        Self {
            tp: TYPE_LONG_LONG,
            flen: 20,
            ..Self::default()
        }
    }

    pub fn new_double() -> Self {
        Self {
            tp: TYPE_DOUBLE,
            flen: 22,
            decimal: -1,
            ..Self::default()
        }
    }

    pub fn new_string() -> Self {
        Self {
            tp: TYPE_VAR_STRING,
            flen: -1,
            ..Self::default()
        }
    }

    pub fn new_newdecimal() -> Self {
        Self {
            tp: TYPE_NEW_DECIMAL,
            flen: 10,
            decimal: 0,
            ..Self::default()
        }
    }

    pub fn type_name(&self) -> String {
        let base = match self.tp {
            TYPE_TINY => "tinyint",
            TYPE_SHORT => "smallint",
            TYPE_LONG | TYPE_INT24 => "int",
            TYPE_LONG_LONG => "bigint",
            TYPE_FLOAT => "float",
            TYPE_DOUBLE => "double",
            TYPE_NEW_DECIMAL | TYPE_DECIMAL => "decimal",
            TYPE_VAR_STRING | TYPE_VARCHAR | TYPE_STRING => {
                if self.flen > 0 {
                    return format!("varchar({})", self.flen);
                }
                "varchar"
            }
            TYPE_BLOB | TYPE_TINY_BLOB | TYPE_MEDIUM_BLOB | TYPE_LONG_BLOB => "blob",
            TYPE_DATE => "date",
            TYPE_DATETIME => "datetime",
            TYPE_TIMESTAMP => "timestamp",
            TYPE_JSON => "json",
            _ => "varchar",
        };
        if self.unsigned {
            format!("{base} unsigned")
        } else {
            base.to_string()
        }
    }

    pub fn eval_type(&self) -> EvalType {
        match self.tp {
            TYPE_TINY | TYPE_SHORT | TYPE_LONG | TYPE_LONG_LONG | TYPE_INT24 | TYPE_YEAR => {
                EvalType::Int
            }
            TYPE_FLOAT | TYPE_DOUBLE => EvalType::Real,
            TYPE_NEW_DECIMAL | TYPE_DECIMAL => EvalType::Decimal,
            TYPE_DATETIME | TYPE_TIMESTAMP | TYPE_DATE => EvalType::Datetime,
            _ => EvalType::String,
        }
    }

    /// MySQL UNSIGNED (`flag & 32`). `unsigned` is serde-skipped so meta
    /// round-trips rely on `flag`.
    pub fn is_unsigned(&self) -> bool {
        self.unsigned || (self.flag & 32) != 0
    }
}

pub const TYPE_DECIMAL: u8 = 0;
pub const TYPE_TINY: u8 = 1;
pub const TYPE_SHORT: u8 = 2;
pub const TYPE_LONG: u8 = 3;
pub const TYPE_FLOAT: u8 = 4;
pub const TYPE_DOUBLE: u8 = 5;
pub const TYPE_NULL: u8 = 6;
pub const TYPE_TIMESTAMP: u8 = 7;
pub const TYPE_LONG_LONG: u8 = 8;
pub const TYPE_INT24: u8 = 9;
pub const TYPE_DATE: u8 = 10;
pub const TYPE_DURATION: u8 = 11;
pub const TYPE_DATETIME: u8 = 12;
pub const TYPE_YEAR: u8 = 13;
pub const TYPE_NEW_DATE: u8 = 14;
pub const TYPE_VARCHAR: u8 = 15;
pub const TYPE_BIT: u8 = 16;
pub const TYPE_JSON: u8 = 0xf5;
pub const TYPE_NEW_DECIMAL: u8 = 0xf6;
pub const TYPE_ENUM: u8 = 0xf7;
pub const TYPE_SET: u8 = 0xf8;
pub const TYPE_TINY_BLOB: u8 = 0xf9;
pub const TYPE_MEDIUM_BLOB: u8 = 0xfa;
pub const TYPE_LONG_BLOB: u8 = 0xfb;
pub const TYPE_BLOB: u8 = 0xfc;
pub const TYPE_VAR_STRING: u8 = 0xfd;
pub const TYPE_STRING: u8 = 0xfe;
