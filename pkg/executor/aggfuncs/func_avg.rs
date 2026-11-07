use super::aggfuncs::{AggFunc, remember_distinct};
use sealdb_errno::Result;
use sealdb_types::Datum;
use sealdb_types::mydecimal::MyDecimal;
use std::collections::HashSet;

#[derive(Default)]
pub struct Avg {
    sum: f64,
    count: i64,
    distinct: bool,
    seen: HashSet<String>,
    /// Fractional scale of the non-float inputs seen so far. TiDB types
    /// `avg(int)` as `decimal(…, 4)` and `avg(decimal(x, s))` as
    /// `decimal(…, s + 4)`.
    frac: Option<i32>,
    float_input: bool,
}

impl Avg {
    pub fn distinct() -> Self {
        Self {
            distinct: true,
            ..Self::default()
        }
    }
}

impl AggFunc for Avg {
    fn update(&mut self, value: Option<&Datum>) -> Result<()> {
        let Some(v) = value else {
            return Ok(());
        };
        if v.is_null() {
            return Ok(());
        }
        if self.distinct && !remember_distinct(&mut self.seen, v) {
            return Ok(());
        }
        match v {
            Datum::F64(_) => self.float_input = true,
            Datum::Decimal(d) => {
                let frac = d.frac_digits();
                self.frac = Some(self.frac.map_or(frac, |f| f.max(frac)));
            }
            _ => {
                self.frac.get_or_insert(0);
            }
        }
        self.sum += v.as_f64()?;
        self.count += 1;
        Ok(())
    }

    fn finalize(&self) -> Result<Datum> {
        if self.count == 0 {
            return Ok(Datum::Null);
        }
        let avg = self.sum / self.count as f64;
        if self.float_input {
            return Ok(Datum::F64(avg));
        }
        // `avg(int)` → decimal(14, 4); decimal input keeps its scale + 4.
        let scale = (self.frac.unwrap_or(0) + 4).clamp(0, 30) as usize;
        let mut text = format!("{avg:.scale$}");
        if text.starts_with("-0.") && text[3..].bytes().all(|b| b == b'0') {
            text = text.trim_start_matches('-').to_string();
        }
        Ok(Datum::Decimal(MyDecimal::parse(&text)?))
    }
}
