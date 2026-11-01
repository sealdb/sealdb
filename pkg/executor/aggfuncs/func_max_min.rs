use super::aggfuncs::{AggFunc, remember_distinct};
use sealdb_errno::Result;
use sealdb_types::Datum;
use std::collections::HashSet;

pub struct Extremum {
    current: Option<Datum>,
    is_max: bool,
    distinct: bool,
    seen: HashSet<String>,
}

impl Extremum {
    pub fn max() -> Self {
        Self {
            current: None,
            is_max: true,
            distinct: false,
            seen: HashSet::new(),
        }
    }

    pub fn min() -> Self {
        Self {
            current: None,
            is_max: false,
            distinct: false,
            seen: HashSet::new(),
        }
    }

    pub fn max_distinct() -> Self {
        Self {
            distinct: true,
            ..Self::max()
        }
    }

    pub fn min_distinct() -> Self {
        Self {
            distinct: true,
            ..Self::min()
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
        if self.distinct && !remember_distinct(&mut self.seen, v) {
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
