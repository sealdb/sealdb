use super::aggfuncs::AggFunc;
use sealdb_errno::Result;
use sealdb_types::Datum;

pub struct Extremum {
    current: Option<Datum>,
    is_max: bool,
}

impl Extremum {
    pub fn max() -> Self {
        Self {
            current: None,
            is_max: true,
        }
    }

    pub fn min() -> Self {
        Self {
            current: None,
            is_max: false,
        }
    }
}

impl AggFunc for Extremum {
    fn update(&mut self, value: Option<&Datum>) -> Result<()> {
        let Some(v) = value else {
            return Ok(());
        };
        if v.is_null() {
            return Ok(());
        }
        match &self.current {
            None => self.current = Some(v.clone()),
            Some(cur) => {
                let ord = v.compare(cur);
                if (self.is_max && ord.is_gt()) || (!self.is_max && ord.is_lt()) {
                    self.current = Some(v.clone());
                }
            }
        }
        Ok(())
    }

    fn finalize(&self) -> Result<Datum> {
        Ok(self.current.clone().unwrap_or(Datum::Null))
    }
}
