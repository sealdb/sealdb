#![forbid(unsafe_code)]

pub mod datum;
pub mod field_type;
pub mod mydecimal;
pub mod time;

pub use datum::Datum;
pub use field_type::{EvalType, FieldType};
pub use mydecimal::MyDecimal;
pub use time::Time;

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
}
