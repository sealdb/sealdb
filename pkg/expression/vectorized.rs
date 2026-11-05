//! Vectorized expression evaluation surface (TiDB `vectorized.go` / `*_vec.go`).
//!
//! TDD target: column-at-a-time `VecEval` over [`sealdb_util::chunk::Chunk`].
//! Scalar [`crate::evaluator`] remains the production path until suites force
//! this on; stubs exist so package/file parity and call sites can land early.

use sealdb_errno::{Error, Result};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;

/// TiDB-shaped vectorized eval hook. Not yet wired into the executor pipeline.
pub trait VecEval {
    fn vectorized(&self) -> bool {
        false
    }

    fn vec_eval(&self, _input: &Chunk, _output: &mut [Datum]) -> Result<()> {
        Err(Error::unsupported("VecEval"))
    }
}

/// Placeholder expression node for future columnar binding.
#[derive(Clone, Debug, Default)]
pub struct VecExprStub;

impl VecEval for VecExprStub {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_reports_not_vectorized() {
        let s = VecExprStub;
        assert!(!s.vectorized());
        assert!(s.vec_eval(&Chunk::default(), &mut []).is_err());
    }
}
