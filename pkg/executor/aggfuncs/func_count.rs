use super::aggfuncs::AggFunc;
use sealdb_errno::Result;
use sealdb_types::Datum;

#[derive(Default)]
pub struct Count {
    count: i64,
    star: bool,
}

impl Count {
    pub fn new(star: bool) -> Self {
        Self { count: 0, star }
    }
}

impl AggFunc for Count {
    fn update(&mut self, value: Option<&Datum>) -> Result<()> {
        if self.star {
            self.count += 1;
            return Ok(());
        }
        if let Some(v) = value
            && !v.is_null()
        {
            self.count += 1;
        }
        Ok(())
    }

    fn finalize(&self) -> Result<Datum> {
        Ok(Datum::I64(self.count))
    }
}
