use super::aggfuncs::AggFunc;
use sealdb_errno::Result;
use sealdb_types::Datum;

#[derive(Default)]
pub struct Sum {
    sum: f64,
    seen: bool,
}

impl AggFunc for Sum {
    fn update(&mut self, value: Option<&Datum>) -> Result<()> {
        let Some(v) = value else {
            return Ok(());
        };
        if v.is_null() {
            return Ok(());
        }
        self.sum += v.as_f64()?;
        self.seen = true;
        Ok(())
    }

    fn finalize(&self) -> Result<Datum> {
        if self.seen {
            if self.sum.fract() == 0.0 && self.sum.is_finite() {
                Ok(Datum::I64(self.sum as i64))
            } else {
                Ok(Datum::F64(self.sum))
            }
        } else {
            Ok(Datum::Null)
        }
    }
}
