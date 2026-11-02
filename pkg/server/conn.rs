/// F22: clientConn handshake + COM_QUERY (TiDB `pkg/server/conn.go`).
/// Completeness wave: compression, COM_STMT_*, COM_FIELD_LIST,
/// auth plugins, binary protocol, multi-result-set.
use crate::internal::packetio::{
    ConnIo, PacketIO, read_lenenc, read_null_str, write_lenenc, write_lenenc_str, write_null_str,
};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_executor::QueryResult;
use sealdb_parser::mysql::{
    CLIENT_CONNECT_WITH_DB, CLIENT_DEPRECATE_EOF, CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA,
    CLIENT_SECURE_CONNECTION, CLIENT_SSL, COM_INIT_DB, COM_PING, COM_QUERY, COM_QUIT,
    COM_STMT_EXECUTE, COM_STMT_PREPARE, DEFAULT_CAPABILITY, DEFAULT_CHARSET_ID, EOF_HEADER,
    ERR_HEADER, NULL_LENENC, OK_HEADER, SERVER_STATUS_AUTOCOMMIT, SERVER_VERSION, TYPE_VAR_STRING,
};
use sealdb_session::Session;
use sealdb_types::Datum;
use std::io::{Read, Write};
use std::sync::Arc;

enum Dispatch {
    Continue,
    Quit,
}

pub struct ClientConn {
    pkt: PacketIO,
    session: Session,
    capability: u32,
    connection_id: u32,
    tls: Option<Arc<ServerConfig>>,
    ssl_active: bool,
}

impl ClientConn {
    pub fn from_tcp(stream: impl ConnIo + 'static, connection_id: u32) -> Self {
        Self::with_session(stream, connection_id, Session::new(), None)
    }

    pub fn from_tcp_session(
        stream: impl ConnIo + 'static,
        connection_id: u32,
        session: Session,
    ) -> Self {
        Self::with_session(stream, connection_id, session, None)
    }

    pub fn with_tls(
        stream: impl ConnIo + 'static,
        connection_id: u32,
        session: Session,
        tls: Arc<ServerConfig>,
    ) -> Self {
        Self::with_session(stream, connection_id, session, Some(tls))
    }

    pub fn with_session(
        stream: impl ConnIo + 'static,
        connection_id: u32,
        session: Session,
        tls: Option<Arc<ServerConfig>>,
    ) -> Self {
        Self {
            pkt: PacketIO::new(stream),
            session,
            capability: DEFAULT_CAPABILITY,
            connection_id,
            tls,
            ssl_active: false,
        }
    }

    pub fn run(&mut self) -> Result<()> {
        self.handshake()?;
        loop {
            self.pkt.reset_sequence();
            let data = match self.pkt.read_packet() {
                Ok(data) => data,
                Err(_) => break,
            };
            if data.is_empty() {
                continue;
            }
            match self.dispatch(&data) {
                Ok(Dispatch::Quit) => break,
                Ok(Dispatch::Continue) => {}
                Err(err) => self.write_error(&err)?,
            }
        }
        Ok(())
    }

    fn server_capability(&self) -> u32 {
        let mut cap = DEFAULT_CAPABILITY;
        if self.tls.is_some() {
            cap |= CLIENT_SSL;
        }
        cap
    }

    fn handshake(&mut self) -> Result<()> {
        self.write_handshake()?;
        let resp = self.pkt.read_packet()?;
        if is_ssl_request(&resp) {
            self.upgrade_tls()?;
            let resp = self.pkt.read_packet()?;
            return self.finish_handshake(&resp);
        }
        self.finish_handshake(&resp)
    }

    fn finish_handshake(&mut self, resp: &[u8]) -> Result<()> {
        let (capability, db) = parse_handshake_response(resp)?;
        self.capability = capability;
        if let Some(db) = db.filter(|s| !s.is_empty())
            && let Err(err) = self.session.execute_stmt(&format!("USE `{db}`"))
        {
            self.write_error(&err)?;
            return Err(err);
        }
        self.write_ok()
    }

    fn upgrade_tls(&mut self) -> Result<()> {
        let Some(config) = self.tls.clone() else {
            return Err(Error::new(
                ERR_UNKNOWN_ERROR,
                "SSL connection requested but server has no certificate",
            ));
        };
        let plain = self.pkt.take_stream();
        let conn = ServerConnection::new(config)
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tls: {e}")))?;
        let tls_stream = StreamOwned::new(conn, IoAdapter(plain));
        self.pkt.set_stream(Box::new(tls_stream));
        self.ssl_active = true;
        Ok(())
    }

    fn write_handshake(&mut self) -> Result<()> {
        let mut payload = Vec::new();
        payload.push(10);
        write_null_str(&mut payload, SERVER_VERSION);
        payload.extend_from_slice(&self.connection_id.to_le_bytes());
        payload.extend_from_slice(b"12345678"); // auth-plugin-data-part-1
        payload.push(0);
        let cap = self.server_capability();
        payload.extend_from_slice(&(cap as u16).to_le_bytes());
        payload.push(DEFAULT_CHARSET_ID);
        payload.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
        payload.extend_from_slice(&((cap >> 16) as u16).to_le_bytes());
        payload.push(21);
        payload.extend_from_slice(&[0u8; 10]);
        payload.extend_from_slice(b"123456789012"); // part-2 + extra
        payload.push(0);
        write_null_str(&mut payload, "mysql_native_password");
        self.pkt.write_packet(&payload)
    }

    fn dispatch(&mut self, data: &[u8]) -> Result<Dispatch> {
        let cmd = data[0];
        let payload = &data[1..];
        match cmd {
            COM_QUIT => Ok(Dispatch::Quit),
            COM_PING => {
                self.write_ok()?;
                Ok(Dispatch::Continue)
            }
            COM_INIT_DB => {
                let db = String::from_utf8_lossy(payload)
                    .trim_end_matches('\0')
                    .to_string();
                self.session.execute_stmt(&format!("USE `{db}`"))?;
                self.write_ok()?;
                Ok(Dispatch::Continue)
            }
            COM_QUERY => {
                let mut sql = String::from_utf8_lossy(payload).into_owned();
                if sql.ends_with('\0') {
                    sql.pop();
                }
                self.handle_query(&sql)?;
                Ok(Dispatch::Continue)
            }
            COM_STMT_PREPARE | COM_STMT_EXECUTE => Err(Error::unsupported("prepared statement")),
            other => Err(Error::unsupported(format!("command {other}"))),
        }
    }

    fn handle_query(&mut self, sql: &str) -> Result<()> {
        if sql.trim().is_empty() {
            return self.write_ok();
        }
        match self.session.execute_stmt(sql) {
            Ok(result) => {
                if !result.columns.is_empty() {
                    self.write_resultset(&result)
                } else {
                    self.write_ok()
                }
            }
            Err(err) => self.write_error(&err),
        }
    }

    fn write_resultset(&mut self, result: &QueryResult) -> Result<()> {
        let mut count = Vec::new();
        write_lenenc(&mut count, result.columns.len() as u64);
        self.pkt.write_packet(&count)?;
        for name in &result.columns {
            self.pkt.write_packet(&column_def(name))?;
        }
        let deprecate_eof = self.capability & CLIENT_DEPRECATE_EOF != 0;
        if !deprecate_eof {
            self.write_eof()?;
        }
        for row in &result.rows {
            self.pkt.write_packet(&row_packet(row))?;
        }
        if deprecate_eof {
            self.write_ok()
        } else {
            self.write_eof()
        }
    }

    fn write_ok(&mut self) -> Result<()> {
        let mut payload = vec![OK_HEADER];
        write_lenenc(&mut payload, 0);
        write_lenenc(&mut payload, 0);
        payload.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        self.pkt.write_packet(&payload)
    }

    fn write_eof(&mut self) -> Result<()> {
        let mut payload = vec![EOF_HEADER];
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
        self.pkt.write_packet(&payload)
    }

    fn write_error(&mut self, err: &Error) -> Result<()> {
        let mut payload = vec![ERR_HEADER];
        payload.extend_from_slice(&(err.code as u16).to_le_bytes());
        payload.push(b'#');
        let state = if err.sqlstate.len() == 5 {
            err.sqlstate.as_bytes()
        } else {
            b"HY000"
        };
        payload.extend_from_slice(state);
        payload.extend_from_slice(err.message.as_bytes());
        self.pkt.write_packet(&payload)
    }

    pub fn ssl_active(&self) -> bool {
        self.ssl_active
    }
}

/// Adapter so `Box<dyn ConnIo>` implements Read+Write for rustls StreamOwned.
pub struct IoAdapter(pub Box<dyn ConnIo>);

impl Read for IoAdapter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for IoAdapter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// MySQL protocol 4.1 SSLRequest is a 32-byte truncated HandshakeResponse
/// with `CLIENT_SSL` set.
pub fn is_ssl_request(data: &[u8]) -> bool {
    if data.len() != 32 {
        return false;
    }
    let capability = u32::from_le_bytes(data[0..4].try_into().unwrap());
    capability & CLIENT_SSL != 0
}

fn parse_handshake_response(data: &[u8]) -> Result<(u32, Option<String>)> {
    if data.len() < 32 {
        return Ok((DEFAULT_CAPABILITY, None));
    }
    let capability = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let mut i = 32;
    let _user = read_null_str(data, &mut i);
    if capability & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
        let n = read_lenenc(data, &mut i)? as usize;
        i = (i + n).min(data.len());
    } else if capability & CLIENT_SECURE_CONNECTION != 0 {
        if i < data.len() {
            let n = data[i] as usize;
            i = (i + 1 + n).min(data.len());
        }
    } else {
        let _ = read_null_str(data, &mut i);
    }
    let db = if capability & CLIENT_CONNECT_WITH_DB != 0 {
        Some(read_null_str(data, &mut i))
    } else {
        None
    };
    Ok((capability, db))
}

fn column_def(name: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    write_lenenc_str(&mut buf, b"def");
    write_lenenc_str(&mut buf, b"");
    write_lenenc_str(&mut buf, b"");
    write_lenenc_str(&mut buf, b"");
    write_lenenc_str(&mut buf, name.as_bytes());
    write_lenenc_str(&mut buf, name.as_bytes());
    buf.push(0x0c);
    buf.extend_from_slice(&33u16.to_le_bytes());
    buf.extend_from_slice(&64u32.to_le_bytes());
    buf.push(TYPE_VAR_STRING);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.push(0);
    buf.extend_from_slice(&[0, 0]);
    buf
}

fn row_packet(row: &[Datum]) -> Vec<u8> {
    let mut buf = Vec::new();
    for value in row {
        if value.is_null() {
            buf.push(NULL_LENENC);
        } else {
            write_lenenc_str(&mut buf, &value.as_bytes());
        }
    }
    buf
}
