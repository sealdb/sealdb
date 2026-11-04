//! TiDB `pkg/store/copr/mpp_probe.go` — TiFlash availability via PD store labels.

use super::pd::{PdStoreInfo, pd_list_stores};
use sealdb_errno::Result;

/// Probe TiFlash stores registered in PD (`engine=tiflash` label).
#[derive(Clone, Debug, Default)]
pub struct MppProbe {
    pub pd_endpoints: Vec<String>,
}

impl MppProbe {
    pub fn new(pd_endpoints: Vec<String>) -> Self {
        Self { pd_endpoints }
    }

    /// All Up TiFlash stores, or empty when none / PD unreachable.
    pub fn list_tiflash_stores(&self) -> Result<Vec<PdStoreInfo>> {
        let stores = pd_list_stores(&self.pd_endpoints)?;
        Ok(stores
            .into_iter()
            .filter(|s| s.is_tiflash() && s.is_up())
            .collect())
    }

    pub fn tiflash_available(&self) -> bool {
        self.list_tiflash_stores()
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copr::pd::{PdStoreInfo, PdStoreLabel};

    #[test]
    fn test_is_tiflash_label() {
        let tikv = PdStoreInfo {
            id: 1,
            address: "127.0.0.1:20160".into(),
            state_name: "Up".into(),
            labels: vec![],
        };
        assert!(!tikv.is_tiflash());
        let flash = PdStoreInfo {
            id: 2,
            address: "127.0.0.1:3930".into(),
            state_name: "Up".into(),
            labels: vec![PdStoreLabel {
                key: "engine".into(),
                value: "tiflash".into(),
            }],
        };
        assert!(flash.is_tiflash());
        assert!(flash.is_up());
    }

    #[test]
    fn test_mpp_probe_if_pd_available() {
        let probe = MppProbe::new(vec!["127.0.0.1:2379".to_string()]);
        match probe.list_tiflash_stores() {
            Ok(stores) => {
                // Cluster may have zero TiFlash nodes; just ensure call succeeds.
                let _ = stores;
                let _ = probe.tiflash_available();
            }
            Err(err) => eprintln!("skip mpp probe: {err}"),
        }
    }
}
