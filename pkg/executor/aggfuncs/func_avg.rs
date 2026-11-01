use super::aggfuncs::AggFunc;
use sealdb_errno::Result;
use sealdb_types::Datum;

#[derive(Default)]
pub struct Avg {
    sum: f64,
    count: i64,
}

impl AggFunc for Avg {
    fn update(&mut self, value: Option<&Datum>) -> Result<()> {
        let Some(v) = value else {
            return Ok(());
        };
        if v.is_null() {
            return Ok(());
        }
        self.sum += v.as_f64()?;
        self.count += 1;
        Ok(())
    }

    fn finalize(&self) -> Result<Datum> {
        if self.count == 0 {
            Ok(Datum::Null)
        } else {
            Ok(Datum::F64(self.sum / self.count as f64))
        }
    }
}
