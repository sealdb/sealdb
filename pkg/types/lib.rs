#![forbid(unsafe_code)]

pub mod datum;
pub mod field_type;
pub mod mydecimal;
pub mod time;

pub use datum::Datum;
pub use datum::EnumValue;
pub use field_type::{
    EvalType, FieldType, TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_DECIMAL, TYPE_DOUBLE,
    TYPE_DURATION, TYPE_FLOAT, TYPE_INT24, TYPE_LONG, TYPE_LONG_BLOB, TYPE_LONG_LONG,
    TYPE_MEDIUM_BLOB, TYPE_NEW_DECIMAL, TYPE_SHORT, TYPE_STRING, TYPE_TIMESTAMP, TYPE_TINY,
    TYPE_TINY_BLOB, TYPE_VAR_STRING, TYPE_VARCHAR, TYPE_YEAR,
};
pub use mydecimal::MyDecimal;
pub use time::{
    Time, TimeType, parse_date_literal, parse_datetime_literal, parse_duration_string,
    parse_time_literal, parse_timestamp_literal,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_datum_compare_null_and_int() {
        assert!(Datum::Null.compare(&Datum::I64(1)).is_lt());
        assert_eq!(
            Datum::I64(1).compare(&Datum::I64(1)),
            std::cmp::Ordering::Equal
        );
        assert!(Datum::I64(-1).compare(&Datum::I64(1)).is_lt());
    }

    #[test]
    fn test_time_packed_roundtrip() {
        let t = Time::new(2024, 1, 2, 3, 4, 5);
        assert_eq!(Time::from_packed_u64(t.to_packed_u64()), t);
    }

    #[test]
    fn test_decimal_parse() {
        let d = MyDecimal::parse("12.50").unwrap();
        assert_eq!(d.to_string(), "12.50");
        assert_eq!(d.to_i64().unwrap(), 12);
    }

    /// Bytes lifted from TiDB's `idx_3(col_0)` histogram
    /// (`decimal(10,0)`, payload only — the header is added by the index codec).
    #[test]
    fn test_decimal_encode_bin_matches_tidb() {
        let cases: [(&str, [u8; 5]); 3] = [
            ("0", [0x80, 0x00, 0x00, 0x00, 0x00]),
            ("8", [0x80, 0x00, 0x00, 0x00, 0x08]),
            ("994677", [0x80, 0x00, 0x0f, 0x2d, 0x75]),
        ];
        for (text, expected) in cases {
            let d = MyDecimal::parse(text).unwrap();
            assert_eq!(d.encode_bin(10, 0).unwrap(), expected, "{text}");
        }
        // Negative values complement every byte before the sign bit flip.
        let d = MyDecimal::parse("-1").unwrap();
        assert_eq!(
            d.encode_bin(10, 0).unwrap(),
            vec![0x7f, 0xff, 0xff, 0xff, 0xfe]
        );
        // `decimal(10,2)`: the trailing group keeps two digits.
        let d = MyDecimal::parse("12.50").unwrap();
        assert_eq!(d.encode_bin(10, 2).unwrap(), vec![0x80, 0, 0, 12, 50]);
    }
}
