use super::aggfuncs::{AggFunc, remember_distinct};
use sealdb_errno::Result;
use sealdb_types::Datum;
use std::collections::HashSet;

/// `BIT_AND` / `BIT_OR` / `BIT_XOR` over the `uint64` view of the argument.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BitKind {
    And,
    Or,
    Xor,
}

pub struct BitAgg {
    kind: BitKind,
    acc: u64,
    distinct: bool,
    distinct_seen: HashSet<String>,
}

impl BitAgg {
    pub fn new(kind: BitKind) -> Self {
        Self {
            kind,
            // TiDB defaults: BIT_AND starts from all ones, BIT_OR/BIT_XOR from 0.
            acc: match kind {
                BitKind::And => u64::MAX,
                BitKind::Or | BitKind::Xor => 0,
            },
            distinct: false,
            distinct_seen: HashSet::new(),
        }
    }

    pub fn distinct(kind: BitKind) -> Self {
        Self {
            distinct: true,
            ..Self::new(kind)
        }
    }
}

impl AggFunc for BitAgg {
    fn update(&mut self, value: Option<&Datum>) -> Result<()> {
        let Some(v) = value else {
            return Ok(());
        };
        if v.is_null() {
            return Ok(());
        }
        if self.distinct && !remember_distinct(&mut self.distinct_seen, v) {
            return Ok(());
        }
        let bits = v.as_f64()? as i64 as u64;
        self.acc = match self.kind {
            BitKind::And => self.acc & bits,
            BitKind::Or => self.acc | bits,
            BitKind::Xor => self.acc ^ bits,
        };
        Ok(())
    }

    fn finalize(&self) -> Result<Datum> {
        // An empty input still returns the identity element (TiDB
        // `AggFuncDesc.GetDefaultValue`).
        Ok(Datum::U64(self.acc))
    }
}
