//! TiDB `pkg/planner/cascades/implementation_rules.go`.

use super::memo::{GroupExpr, Memo};
use std::collections::HashSet;

/// Logical→physical implementation rules (fingerprint MVB).
#[derive(Clone, Debug, Default)]
pub struct ImplementationRules;

fn fnv_label(label: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in label {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Logical Selection → PhysicalSelection marker fingerprint.
pub fn logical_selection_fingerprint() -> u64 {
    fnv_label(b"LogicalSelection")
}

pub fn physical_selection_fingerprint() -> u64 {
    fnv_label(b"PhysicalSelection")
}

/// Logical TableScan → PhysicalTableReader marker fingerprint.
pub fn logical_table_scan_fingerprint() -> u64 {
    fnv_label(b"LogicalTableScan")
}

pub fn physical_table_reader_fingerprint() -> u64 {
    fnv_label(b"PhysicalTableReader")
}

impl ImplementationRules {
    /// Implement known logical operators by inserting physical fingerprints.
    ///
    /// Returns the number of physical exprs added.
    pub fn apply_all(&self, memo: &mut Memo) -> usize {
        let mut added = 0;
        for group in &mut memo.groups {
            let logical: HashSet<u64> = group.exprs.iter().map(|e| e.fingerprint).collect();
            let existing: HashSet<u64> = logical.clone();
            let mut to_add = Vec::new();
            if logical.contains(&logical_selection_fingerprint())
                && !existing.contains(&physical_selection_fingerprint())
            {
                to_add.push(physical_selection_fingerprint());
            }
            if logical.contains(&logical_table_scan_fingerprint())
                && !existing.contains(&physical_table_reader_fingerprint())
            {
                to_add.push(physical_table_reader_fingerprint());
            }
            for fp in to_add {
                group.exprs.push(GroupExpr { fingerprint: fp });
                added += 1;
            }
        }
        added
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cascades::memo::{Group, GroupExpr, Memo};

    #[test]
    fn test_implement_selection_and_table_scan() {
        let mut memo = Memo {
            groups: vec![Group {
                id: 0,
                exprs: vec![
                    GroupExpr {
                        fingerprint: logical_selection_fingerprint(),
                    },
                    GroupExpr {
                        fingerprint: logical_table_scan_fingerprint(),
                    },
                ],
            }],
        };
        let n = ImplementationRules.apply_all(&mut memo);
        assert_eq!(n, 2);
        let fps: HashSet<_> = memo.groups[0].exprs.iter().map(|e| e.fingerprint).collect();
        assert!(fps.contains(&physical_selection_fingerprint()));
        assert!(fps.contains(&physical_table_reader_fingerprint()));
        // Idempotent.
        assert_eq!(ImplementationRules.apply_all(&mut memo), 0);
    }
}
