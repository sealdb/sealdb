#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvalType {
    Int,
    Real,
    Decimal,
    String,
    Datetime,
}

/// MySQL/TiDB `NotNullFlag` (`FieldType.Flag & 1`).
pub const NOT_NULL_FLAG: u32 = 1;

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
    /// `ENUM`/`SET` members in declaration order (TiDB `FieldType.Elems`).
    #[serde(rename = "Elems", default, skip_serializing_if = "Option::is_none")]
    pub elems: Option<Vec<String>>,
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
            elems: None,
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
            TYPE_LONG => "int",
            TYPE_INT24 => "mediumint",
            TYPE_LONG_LONG => "bigint",
            TYPE_FLOAT => "float",
            TYPE_DOUBLE => "double",
            TYPE_NEW_DECIMAL | TYPE_DECIMAL => "decimal",
            TYPE_VAR_STRING | TYPE_VARCHAR | TYPE_STRING => {
                if self.charset == "binary" {
                    let name = if self.tp == TYPE_STRING {
                        "binary"
                    } else {
                        "varbinary"
                    };
                    return if self.flen > 0 {
                        format!("{name}({})", self.flen)
                    } else {
                        name.to_string()
                    };
                }
                if self.flen > 0 {
                    let name = if self.tp == TYPE_STRING { "char" } else { "varchar" };
                    return format!("{name}({})", self.flen);
                }
                if self.flen < 0 {
                    // MySQL `TEXT` is a length-less string type with a
                    // non-binary charset (see `is_lob_type`).
                    return "text".to_string();
                }
                if self.tp == TYPE_STRING {
                    return "char".to_string();
                }
                "varchar"
            }
            TYPE_YEAR => return format!("year({})", self.flen.max(4)),
            TYPE_BLOB | TYPE_TINY_BLOB | TYPE_MEDIUM_BLOB | TYPE_LONG_BLOB => {
                let size = match self.tp {
                    TYPE_TINY_BLOB => "tiny",
                    TYPE_MEDIUM_BLOB => "medium",
                    TYPE_LONG_BLOB => "long",
                    _ => "",
                };
                // The utf8 charsets spell the same storage class as TEXT.
                return if self.charset == "binary" {
                    format!("{size}blob")
                } else {
                    format!("{size}text")
                };
            }
            TYPE_DATE => "date",
            TYPE_DATETIME => "datetime",
            TYPE_TIMESTAMP => "timestamp",
            TYPE_DURATION => "time",
            TYPE_JSON => "json",
            TYPE_BIT => return format!("bit({})", self.flen.max(1)),
            TYPE_ENUM => return self.elems_type_name("enum"),
            TYPE_SET => return self.elems_type_name("set"),
            _ => "varchar",
        };
        let mut rendered = if matches!(self.tp, TYPE_NEW_DECIMAL | TYPE_DECIMAL) && self.flen > 0 {
            // TiDB always spells DECIMAL as `decimal(M,D)`.
            format!("{base}({},{})", self.flen, self.decimal.max(0))
        } else if matches!(self.tp, TYPE_FLOAT | TYPE_DOUBLE) && self.flen > 0 && self.decimal > 0 {
            // `real(20,12)` is a DOUBLE that TiDB spells `double(20,12)`.
            format!("{base}({},{})", self.flen, self.decimal)
        } else if matches!(self.tp, TYPE_DATETIME | TYPE_TIMESTAMP | TYPE_DURATION)
            && self.decimal > 0
        {
            // `datetime(3)` / `time(3)` — TiDB prints the fsp only when > 0.
            format!("{base}({})", self.decimal)
        } else {
            base.to_string()
        };
        if self.is_unsigned() {
            rendered.push_str(" unsigned");
        }
        rendered
    }

    /// TiDB `FieldType.HandleWrongValue` numbering for an ENUM/SET label:
    /// 1-based member index for ENUM, member bitmask for SET (0 when empty).
    pub fn enum_set_number(&self, label: &[u8]) -> u64 {
        let Some(elems) = self.elems.as_deref() else {
            return 0;
        };
        if label.is_empty() {
            return 0;
        }
        let s = String::from_utf8_lossy(label);
        if self.tp == TYPE_ENUM {
            return elems
                .iter()
                .position(|e| e.eq_ignore_ascii_case(&s))
                .map(|i| i as u64 + 1)
                .unwrap_or(0);
        }
        let mut bits = 0u64;
        for part in s.split(',') {
            if let Some(i) = elems.iter().position(|e| e.eq_ignore_ascii_case(part)) {
                bits |= 1u64 << i;
            }
        }
        bits
    }

    /// TiDB renders `enum('a','b')` / `set('a','b')` in SHOW/DESCRIBE output.
    fn elems_type_name(&self, kind: &str) -> String {
        let Some(elems) = &self.elems else {
            return kind.to_string();
        };
        let quoted: Vec<String> = elems
            .iter()
            .map(|e| format!("'{}'", e.replace('\\', "\\\\").replace('\'', "''")))
            .collect();
        format!("{kind}({})", quoted.join(","))
    }

    pub fn eval_type(&self) -> EvalType {
        match self.tp {
            TYPE_TINY | TYPE_SHORT | TYPE_LONG | TYPE_LONG_LONG | TYPE_INT24 | TYPE_YEAR
            | TYPE_BIT => EvalType::Int,
            TYPE_FLOAT | TYPE_DOUBLE => EvalType::Real,
            TYPE_NEW_DECIMAL | TYPE_DECIMAL => EvalType::Decimal,
            TYPE_DATETIME | TYPE_TIMESTAMP | TYPE_DATE | TYPE_DURATION => EvalType::Datetime,
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
