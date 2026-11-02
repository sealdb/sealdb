//! Cost Model Version 2 (MVB).
//!
//! TiDB-shaped separation of CPU / memory / network / scan factors into
//! [`CostVer2`] values. JoinReorder (two-table + n-table DP) and EXPLAIN
//! `estCost` use these helpers. Full per-operator GetPlanCostVer2,
//! concurrency, and TiFlash/MPP factors stay deferred.
//!
//! Default factor values mirror TiDB `defaultVer2Factors` (CPU/mem/net/scan).
//! Override via [`CostFactors`] (tests / future session vars such as
//! `tidb_opt_cpu_factor`).

use std::fmt;

/// Named cost factor (TiDB `CostVer2Factor`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CostVer2Factor {
    pub name: &'static str,
    pub value: f64,
}

impl fmt::Display for CostVer2Factor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.name, self.value)
    }
}

/// Accumulated v2 cost (TiDB `CostVer2` without trace).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CostVer2 {
    cost: f64,
}

impl CostVer2 {
    pub const ZERO: Self = Self { cost: 0.0 };

    pub fn new(cost: f64) -> Self {
        Self { cost }
    }

    pub fn get_cost(self) -> f64 {
        self.cost.max(0.0)
    }

    pub fn sum(parts: &[CostVer2]) -> Self {
        Self {
            cost: parts.iter().map(|c| c.cost).sum(),
        }
    }
}

/// Tunable factor set for Cost Model v2 MVB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CostFactors {
    pub tikv_scan: CostVer2Factor,
    pub tidb_cpu: CostVer2Factor,
    pub tidb_mem: CostVer2Factor,
    pub tidb_kv_net: CostVer2Factor,
}

impl Default for CostFactors {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl CostFactors {
    /// TiDB `defaultVer2Factors` subset used by this MVB.
    pub const DEFAULT: Self = Self {
        tikv_scan: CostVer2Factor {
            name: "tikv_scan_factor",
            value: 40.70,
        },
        tidb_cpu: CostVer2Factor {
            name: "tidb_cpu_factor",
            value: 49.90,
        },
        tidb_mem: CostVer2Factor {
            name: "tidb_mem_factor",
            value: 0.20,
        },
        tidb_kv_net: CostVer2Factor {
            name: "tidb_kv_net_factor",
            value: 3.96,
        },
    };

    pub fn with_cpu(mut self, value: f64) -> Self {
        self.tidb_cpu.value = value;
        self
    }

    pub fn with_mem(mut self, value: f64) -> Self {
        self.tidb_mem.value = value;
        self
    }

    pub fn with_scan(mut self, value: f64) -> Self {
        self.tikv_scan.value = value;
        self
    }

    pub fn with_net(mut self, value: f64) -> Self {
        self.tidb_kv_net.value = value;
        self
    }
}

const DEFAULT_ROW_SIZE: f64 = 8.0;
const MIN_ROW_SIZE: f64 = 2.0;
const N_JOIN_KEYS: f64 = 1.0;

/// `rows * log2(row_size) * scan_factor` (TiDB `scanCostVer2`).
pub fn table_scan_cost_ver2(rows: f64, factors: &CostFactors) -> CostVer2 {
    let row_size = DEFAULT_ROW_SIZE.max(MIN_ROW_SIZE);
    let log_sz = row_size.log2().max(0.0);
    CostVer2::new(rows * log_sz * factors.tikv_scan.value)
}

/// Index lookup: scan + network double-read (MVB).
pub fn index_lookup_cost_ver2(rows: f64, factors: &CostFactors) -> CostVer2 {
    let scan = table_scan_cost_ver2(rows, factors);
    let net = CostVer2::new(rows * DEFAULT_ROW_SIZE * factors.tidb_kv_net.value);
    CostVer2::sum(&[scan, net])
}

/// Hash build: key hash + mem + build CPU (TiDB `hashBuildCostVer2`).
fn hash_build_cost_ver2(build_rows: f64, factors: &CostFactors) -> CostVer2 {
    let row_size = DEFAULT_ROW_SIZE;
    let hash_key = CostVer2::new(build_rows * N_JOIN_KEYS * factors.tidb_cpu.value);
    let hash_mem = CostVer2::new(build_rows * row_size * factors.tidb_mem.value);
    let hash_build = CostVer2::new(build_rows * factors.tidb_cpu.value);
    CostVer2::sum(&[hash_key, hash_mem, hash_build])
}

/// Hash probe: key hash + probe CPU (TiDB `hashProbeCostVer2`).
fn hash_probe_cost_ver2(probe_rows: f64, factors: &CostFactors) -> CostVer2 {
    let hash_key = CostVer2::new(probe_rows * N_JOIN_KEYS * factors.tidb_cpu.value);
    let hash_probe = CostVer2::new(probe_rows * factors.tidb_cpu.value);
    CostVer2::sum(&[hash_key, hash_probe])
}

/// TiDB-style HashJoin v2: `probe` probes, `build` builds.
///
/// `plan-cost = start + probe_child + build_child + hash_build + hash_probe`
pub fn hash_join_cost_ver2(
    probe_rows: f64,
    build_rows: f64,
    probe_cost: CostVer2,
    build_cost: CostVer2,
    factors: &CostFactors,
) -> CostVer2 {
    let start = CostVer2::new(10.0 * 3.0 * factors.tidb_cpu.value);
    let build_hash = hash_build_cost_ver2(build_rows.max(1.0), factors);
    let probe_hash = hash_probe_cost_ver2(probe_rows, factors);
    CostVer2::sum(&[start, probe_cost, build_cost, build_hash, probe_hash])
}

/// Scalar helpers for JoinReorder / DP (default factors).
pub fn table_scan_cost(rows: f64) -> f64 {
    table_scan_cost_ver2(rows, &CostFactors::DEFAULT).get_cost()
}

pub fn hash_join_cost(probe_rows: f64, build_rows: f64, probe_cost: f64, build_cost: f64) -> f64 {
    hash_join_cost_ver2(
        probe_rows,
        build_rows,
        CostVer2::new(probe_cost),
        CostVer2::new(build_cost),
        &CostFactors::DEFAULT,
    )
    .get_cost()
}

/// Estimated HashJoin cost for EXPLAIN `estCost` (default factors, scan children).
pub fn hash_join_est_cost(probe_rows: f64, build_rows: f64) -> f64 {
    let probe = table_scan_cost_ver2(probe_rows, &CostFactors::DEFAULT);
    let build = table_scan_cost_ver2(build_rows, &CostFactors::DEFAULT);
    hash_join_cost_ver2(probe_rows, build_rows, probe, build, &CostFactors::DEFAULT).get_cost()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_cost_uses_scan_factor() {
        let base = table_scan_cost_ver2(100.0, &CostFactors::DEFAULT).get_cost();
        let scaled = table_scan_cost_ver2(100.0, &CostFactors::DEFAULT.with_scan(81.40)).get_cost();
        assert!(base > 0.0);
        assert!(
            (scaled - base * 2.0).abs() < 1e-6,
            "base={base} scaled={scaled}"
        );
    }

    #[test]
    fn test_join_cost_ver2_prefers_cheaper_build() {
        let small = 1.0;
        let big = 100.0;
        let factors = CostFactors::DEFAULT;
        let small_c = table_scan_cost_ver2(small, &factors);
        let big_c = table_scan_cost_ver2(big, &factors);
        let build_small = hash_join_cost_ver2(big, small, big_c, small_c, &factors).get_cost();
        let build_big = hash_join_cost_ver2(small, big, small_c, big_c, &factors).get_cost();
        assert!(
            build_small < build_big,
            "build_small={build_small} build_big={build_big}"
        );
    }

    #[test]
    fn test_high_mem_factor_widens_build_preference() {
        let small = 1.0;
        let big = 100.0;
        let low = CostFactors::DEFAULT.with_mem(0.01);
        let high = CostFactors::DEFAULT.with_mem(10.0);
        let gap = |f: &CostFactors| {
            let s = table_scan_cost_ver2(small, f);
            let b = table_scan_cost_ver2(big, f);
            hash_join_cost_ver2(small, big, s, b, f).get_cost()
                - hash_join_cost_ver2(big, small, b, s, f).get_cost()
        };
        assert!(
            gap(&high) > gap(&low),
            "high={} low={}",
            gap(&high),
            gap(&low)
        );
    }

    #[test]
    fn test_est_cost_helper_positive() {
        let cost = hash_join_est_cost(100.0, 1.0);
        assert!(cost > 0.0);
        assert!(hash_join_est_cost(1.0, 100.0) > cost);
    }
}
