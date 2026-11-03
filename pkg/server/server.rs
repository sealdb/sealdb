/// F22: Server listen/accept + optional TLS (TiDB `pkg/server/server.go` SSL).
/// Completeness wave: connection limits, status HTTP, graceful drain.
use crate::conn::ClientConn;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sealdb_domain::Domain;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_session::Session;
use std::fs;
use std::io::{Cursor, ErrorKind};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

/// PEM cert + key paths used by `Server::with_ssl_config`.
#[derive(Clone, Debug)]
pub struct SslConfig {
    pub cert_path: String,
    pub key_path: String,
}

impl SslConfig {
    pub fn new(cert_path: impl Into<String>, key_path: impl Into<String>) -> Self {
        Self {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
        }
    }

    pub fn load_server_config(&self) -> Result<Arc<ServerConfig>> {
        let cert_pem = fs::read(&self.cert_path).map_err(|e| {
            Error::new(
                ERR_UNKNOWN_ERROR,
                format!("read SSL cert {}: {e}", self.cert_path),
            )
        })?;
        let key_pem = fs::read(&self.key_path).map_err(|e| {
            Error::new(
                ERR_UNKNOWN_ERROR,
                format!("read SSL key {}: {e}", self.key_path),
            )
        })?;
        server_config_from_pem(&cert_pem, &key_pem)
    }
}

pub fn server_config_from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<Arc<ServerConfig>> {
    let certs = load_certs(cert_pem)?;
    let key = load_key(key_pem)?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tls config: {e}")))?;
    Ok(Arc::new(config))
}

fn load_certs(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = Cursor::new(pem);
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("parse cert PEM: {e}")))?;
    if certs.is_empty() {
        return Err(Error::new(ERR_UNKNOWN_ERROR, "no certificates in PEM"));
    }
    Ok(certs)
}

fn load_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>> {
    let mut reader = Cursor::new(pem);
    let key = rustls_pemfile::private_key(&mut reader)
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("parse key PEM: {e}")))?
        .ok_or_else(|| Error::new(ERR_UNKNOWN_ERROR, "no private key in PEM"))?;
    Ok(key)
}

/// True when cert and key files exist.
pub fn ssl_paths_exist(cfg: &SslConfig) -> bool {
    Path::new(&cfg.cert_path).is_file() && Path::new(&cfg.key_path).is_file()
}

pub struct Server {
    listener: TcpListener,
    domain: Arc<Domain>,
    next_id: AtomicU32,
    tls: Option<Arc<ServerConfig>>,
}

impl Server {
    pub fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        Self::bind_with_domain(addr, Arc::new(Domain::new()))
    }

    /// Listen with grant tables enabled (mysql.user auth during handshake).
    pub fn bind_with_grant_table(addr: impl ToSocketAddrs) -> Result<Self> {
        Self::bind_with_domain(addr, Arc::new(Domain::with_grant_table()))
    }

    pub fn bind_with_domain(addr: impl ToSocketAddrs, domain: Arc<Domain>) -> Result<Self> {
        let listener = TcpListener::bind(addr)
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("listen: {e}")))?;
        Ok(Self {
            listener,
            domain,
            next_id: AtomicU32::new(1),
            tls: None,
        })
    }

    /// Enable MySQL `CLIENT_SSL` and TLS upgrade after SSLRequest.
    pub fn with_tls(mut self, tls: Arc<ServerConfig>) -> Self {
        self.tls = Some(tls);
        self
    }

    pub fn with_ssl_config(self, cfg: &SslConfig) -> Result<Self> {
        Ok(self.with_tls(cfg.load_server_config()?))
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.listener
            .local_addr()
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("local addr: {e}")))
    }

    pub fn tls_enabled(&self) -> bool {
        self.tls.is_some()
    }

    pub fn serve(&self) -> Result<()> {
        // Non-blocking accept so SHUTDOWN can exit without a new connection.
        self.listener
            .set_nonblocking(true)
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("set nonblocking: {e}")))?;
        loop {
            if self.domain.is_shutdown_requested() {
                let _ = self.listener.set_nonblocking(false);
                return Ok(());
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Accepted fds inherit nonblocking on some platforms.
                    let _ = stream.set_nonblocking(false);
                    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                    let session = Session::with_domain(Arc::clone(&self.domain));
                    let tls = self.tls.clone();
                    thread::spawn(move || {
                        let mut conn = match tls {
                            Some(tls) => ClientConn::with_tls(stream, id, session, tls),
                            None => ClientConn::from_tcp_session(stream, id, session),
                        };
                        let _ = conn.run();
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    return Err(Error::new(ERR_UNKNOWN_ERROR, format!("accept: {e}")));
                }
            }
        }
    }

    pub fn accept_one(&self) -> Result<()> {
        let (stream, _) = self
            .listener
            .accept()
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("accept: {e}")))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session = Session::with_domain(Arc::clone(&self.domain));
        let tls = self.tls.clone();
        thread::spawn(move || {
            let mut conn = match tls {
                Some(tls) => ClientConn::with_tls(stream, id, session, tls),
                None => ClientConn::from_tcp_session(stream, id, session),
            };
            let _ = conn.run();
        });
        Ok(())
    }

    pub fn domain(&self) -> Arc<Domain> {
        Arc::clone(&self.domain)
    }
}
