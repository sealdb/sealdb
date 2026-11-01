/// F22 core: Server listen/accept (TiDB `pkg/server/server.go`).
/// Completeness wave: TLS, connection limits, status HTTP, graceful drain.
use crate::conn::ClientConn;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_session::Session;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

pub struct Server {
    listener: TcpListener,
    domain: Arc<Domain>,
    next_id: AtomicU32,
}

impl Server {
    pub fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        Self::bind_with_domain(addr, Arc::new(Domain::new()))
    }

    pub fn bind_with_domain(addr: impl ToSocketAddrs, domain: Arc<Domain>) -> Result<Self> {
        let listener = TcpListener::bind(addr)
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("listen: {e}")))?;
        Ok(Self {
            listener,
            domain,
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
        let session = Session::with_domain(Arc::clone(&self.domain));
        std::thread::spawn(move || {
            let mut conn = ClientConn::from_tcp_session(stream, id, session);
            let _ = conn.run();
        });
        Ok(())
    }
}
