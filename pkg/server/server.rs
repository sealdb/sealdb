/// F22 core: Server listen/accept (TiDB `pkg/server/server.go`).
/// Completeness wave: TLS, connection limits, status HTTP, graceful drain.
use crate::conn::ClientConn;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::sync::atomic::{AtomicU32, Ordering};

pub struct Server {
    listener: TcpListener,
    next_id: AtomicU32,
}

impl Server {
    pub fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        let listener = TcpListener::bind(addr)
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("listen: {e}")))?;
        Ok(Self {
            listener,
            next_id: AtomicU32::new(1),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.listener
            .local_addr()
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("local addr: {e}")))
    }

    pub fn serve(&self) -> Result<()> {
        loop {
            self.accept_one()?;
        }
    }

    pub fn accept_one(&self) -> Result<()> {
        let (stream, _) = self
            .listener
            .accept()
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("accept: {e}")))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let mut conn = ClientConn::from_tcp(stream, id);
            let _ = conn.run();
        });
        Ok(())
    }
}
