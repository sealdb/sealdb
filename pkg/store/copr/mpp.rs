//! TiDB `pkg/store/copr/mpp.go` — MPP coordination over TiFlash stores.
//!
//! Dispatch / exchange still deferred; this slice discovers TiFlash peers via PD.

use super::mpp_probe::MppProbe;
use super::pd::PdStoreInfo;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};

/// Minimal MPP client: TiFlash store discovery for later BatchCop/MPP dispatch.
#[derive(Clone, Debug, Default)]
pub struct MppClient {
    probe: MppProbe,
}

impl MppClient {
    pub fn new(pd_endpoints: Vec<String>) -> Self {
        Self {
            probe: MppProbe::new(pd_endpoints),
        }
    }

    pub fn tiflash_stores(&self) -> Result<Vec<PdStoreInfo>> {
        self.probe.list_tiflash_stores()
    }

    pub fn require_tiflash(&self) -> Result<Vec<PdStoreInfo>> {
        let stores = self.tiflash_stores()?;
        if stores.is_empty() {
            return Err(Error::new(
                ERR_UNKNOWN_ERROR,
                "no TiFlash store with label engine=tiflash",
            ));
        }
        Ok(stores)
    }

    pub fn available(&self) -> bool {
        self.probe.tiflash_available()
    }
}
