/// clientConn handshake + COM_QUERY (TiDB `pkg/server/conn.go`).
/// Completeness wave: compression, COM_STMT_*, COM_FIELD_LIST,
/// auth plugins, binary protocol, multi-result-set.
use crate::internal::packetio::{
    ConnIo, PacketIO, read_lenenc, read_null_str, write_lenenc, write_lenenc_str, write_null_str,
};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use sealdb_errno::{ERR_ACCESS_DENIED_ERROR, ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_executor::QueryResult;
use sealdb_parser::ast::UserIdentity;
use sealdb_parser::mysql::{
    CLIENT_COMPRESS, CLIENT_CONNECT_WITH_DB, CLIENT_DEPRECATE_EOF, CLIENT_MULTI_RESULTS,
    CLIENT_MULTI_STATEMENTS, CLIENT_PLUGIN_AUTH, CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA,
    CLIENT_SECURE_CONNECTION, CLIENT_SSL, COM_INIT_DB, COM_PING, COM_QUERY, COM_QUIT,
    COM_STMT_CLOSE, COM_STMT_EXECUTE, COM_STMT_PREPARE, DEFAULT_CAPABILITY, DEFAULT_CHARSET_ID,
    EOF_HEADER, ERR_HEADER, NULL_LENENC, OK_HEADER, SERVER_MORE_RESULTS_EXISTS,
    SERVER_STATUS_AUTOCOMMIT, SERVER_VERSION, TYPE_LONGLONG, TYPE_NULL, TYPE_VAR_STRING,
};
use sealdb_session::Session;
use sealdb_types::Datum;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;

#[derive(Clone)]
struct PreparedStmt {
    sql: String,
    num_params: u16,
}

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
    prepared: HashMap<u32, PreparedStmt>,
    next_stmt_id: u32,
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
            prepared: HashMap::new(),
            next_stmt_id: 1,
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
        let parsed = parse_handshake_response(resp)?;
        self.capability = parsed.capability;
        // Auth plugin: stay on mysql_native_password; switch clients that ask otherwise.
        if !parsed.auth_plugin.is_empty()
            && parsed.auth_plugin != "mysql_native_password"
            && self.capability & CLIENT_PLUGIN_AUTH != 0
        {
            self.write_auth_switch("mysql_native_password")?;
            let _auth_data = self.pkt.read_packet()?;
        }
        let user = if parsed.user.is_empty() {
            "root".to_string()
        } else {
            parsed.user
        };
        let identity = UserIdentity::new(user, "%");
        if let Err(err) = self.authenticate(&identity) {
            self.write_error(&err)?;
            return Err(err);
        }
        self.session.set_current_user(identity);
        if let Some(db) = parsed.db.filter(|s| !s.is_empty())
            && let Err(err) = self.session.execute_stmt(&format!("USE `{db}`"))
        {
            self.write_error(&err)?;
            return Err(err);
        }
        // OK packet is still uncompressed; enable zlib afterwards.
        self.write_ok()?;
        if parsed.capability & CLIENT_COMPRESS != 0
            && self.server_capability() & CLIENT_COMPRESS != 0
        {
            self.pkt.set_compressed(true);
        }
        Ok(())
    }

    fn authenticate(&self, user: &UserIdentity) -> Result<()> {
        let privs = self.session.privilege_manager();
        if privs.skip_grant_table() || privs.user_exists(user) {
            return Ok(());
        }
        Err(Error::new(
            ERR_ACCESS_DENIED_ERROR,
            format!(
                "Access denied for user '{}'@'{}' (using password: NO)",
                user.username, user.hostname
            ),
        ))
    }

    fn write_auth_switch(&mut self, plugin: &str) -> Result<()> {
        let mut payload = vec![0xfe];
        write_null_str(&mut payload, plugin);
        payload.extend_from_slice(b"12345678901234567890");
        payload.push(0);
        self.pkt.write_packet(&payload)
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
        // TiDB tags statement errors with the client connection id
        // (`… [conn=17]`, e.g. error 8175 memory quota).
        sealdb_expression::set_connection_id(u64::from(self.connection_id));
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
            COM_STMT_PREPARE => {
                self.handle_stmt_prepare(payload)?;
                Ok(Dispatch::Continue)
            }
            COM_STMT_EXECUTE => {
                self.handle_stmt_execute(payload)?;
                Ok(Dispatch::Continue)
            }
            COM_STMT_CLOSE => {
                if payload.len() >= 4 {
                    let id = u32::from_le_bytes(payload[0..4].try_into().unwrap());
                    self.prepared.remove(&id);
                }
                Ok(Dispatch::Continue)
            }
            other => Err(Error::unsupported(format!("command {other}"))),
        }
    }

    fn handle_stmt_prepare(&mut self, payload: &[u8]) -> Result<()> {
        let sql = String::from_utf8_lossy(payload)
            .trim_end_matches('\0')
            .to_string();
        let num_params = count_placeholders(&sql) as u16;
        let id = self.next_stmt_id;
        self.next_stmt_id = self.next_stmt_id.wrapping_add(1);
        self.prepared.insert(
            id,
            PreparedStmt {
                sql: sql.clone(),
                num_params,
            },
        );
        // COM_STMT_PREPARE OK
        let mut pkt = vec![0x00];
        pkt.extend_from_slice(&id.to_le_bytes());
        pkt.extend_from_slice(&0u16.to_le_bytes()); // num_columns (unknown until execute)
        pkt.extend_from_slice(&num_params.to_le_bytes());
        pkt.push(0); // filler
        pkt.extend_from_slice(&0u16.to_le_bytes()); // warnings
        self.pkt.write_packet(&pkt)?;
        if num_params > 0 {
            for _ in 0..num_params {
                self.pkt.write_packet(&column_def("?"))?;
            }
            if self.capability & CLIENT_DEPRECATE_EOF == 0 {
                self.write_eof()?;
            }
        }
        Ok(())
    }

    fn handle_stmt_execute(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < 9 {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "truncated COM_STMT_EXECUTE"));
        }
        let id = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let stmt = self
            .prepared
            .get(&id)
            .ok_or_else(|| {
                Error::new(
                    ERR_UNKNOWN_ERROR,
                    format!("Unknown prepared statement id {id}"),
                )
            })?
            .clone();
        let params = parse_binary_params(payload, stmt.num_params)?;
        let sql = bind_placeholders(&stmt.sql, &params)?;
        // Domain plan cache keyed by unbound prepared SQL (with `?`).
        match self
            .session
            .execute_cached_stmt_with_key(&sql, Some(stmt.sql.as_str()))
        {
            Ok(result) => {
                if result.columns.is_empty() {
                    self.write_ok()
                } else {
                    self.write_binary_resultset(&result)
                }
            }
            Err(err) => self.write_error(&err),
        }
    }

    fn write_binary_resultset(&mut self, result: &QueryResult) -> Result<()> {
        let mut count = Vec::new();
        write_lenenc(&mut count, result.columns.len() as u64);
        self.pkt.write_packet(&count)?;
        for name in &result.columns {
            self.pkt.write_packet(&column_def(name))?;
        }
        let deprecate_eof = self.capability & CLIENT_DEPRECATE_EOF != 0;
        if !deprecate_eof {
            self.write_eof_status(false)?;
        }
        for row in &result.rows {
            self.pkt.write_packet(&binary_row_packet(row))?;
        }
        if deprecate_eof {
            self.write_ok_status(false)
        } else {
            self.write_eof_status(false)
        }
    }

    fn handle_query(&mut self, sql: &str) -> Result<()> {
        if sql.trim().is_empty() {
            return self.write_ok_status(false);
        }
        // Multi-statements require CLIENT_MULTI_STATEMENTS; otherwise take first only.
        let sql = if self.capability & CLIENT_MULTI_STATEMENTS == 0 {
            sql.split(';').next().unwrap_or(sql)
        } else {
            sql
        };
        match self.session.execute_all(sql) {
            Ok(results) => {
                let multi = self.capability & CLIENT_MULTI_RESULTS != 0 && results.len() > 1;
                for (i, result) in results.iter().enumerate() {
                    let more = multi && i + 1 < results.len();
                    if result.columns.is_empty() {
                        self.write_ok_status(more)?;
                    } else {
                        self.write_resultset_status(result, more)?;
                    }
                }
                Ok(())
            }
            Err(err) => self.write_error(&err),
        }
    }

    fn write_resultset_status(&mut self, result: &QueryResult, more: bool) -> Result<()> {
        let mut count = Vec::new();
        write_lenenc(&mut count, result.columns.len() as u64);
        self.pkt.write_packet(&count)?;
        for name in &result.columns {
            self.pkt.write_packet(&column_def(name))?;
        }
        let deprecate_eof = self.capability & CLIENT_DEPRECATE_EOF != 0;
        if !deprecate_eof {
            self.write_eof_status(more)?;
        }
        for row in &result.rows {
            self.pkt.write_packet(&row_packet(row))?;
        }
        if deprecate_eof {
            self.write_ok_status(more)
        } else {
            self.write_eof_status(more)
        }
    }

    fn write_ok(&mut self) -> Result<()> {
        self.write_ok_status(false)
    }

    fn write_ok_status(&mut self, more: bool) -> Result<()> {
        let mut status = SERVER_STATUS_AUTOCOMMIT;
        if more {
            status |= SERVER_MORE_RESULTS_EXISTS;
        }
        let mut payload = vec![OK_HEADER];
        write_lenenc(&mut payload, 0);
        write_lenenc(&mut payload, 0);
        payload.extend_from_slice(&status.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        self.pkt.write_packet(&payload)
    }

    fn write_eof(&mut self) -> Result<()> {
        self.write_eof_status(false)
    }

    fn write_eof_status(&mut self, more: bool) -> Result<()> {
        let mut status = SERVER_STATUS_AUTOCOMMIT;
        if more {
            status |= SERVER_MORE_RESULTS_EXISTS;
        }
        let mut payload = vec![EOF_HEADER];
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&status.to_le_bytes());
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

struct HandshakeResponse {
    capability: u32,
    user: String,
    db: Option<String>,
    auth_plugin: String,
}

fn parse_handshake_response(data: &[u8]) -> Result<HandshakeResponse> {
    if data.len() < 32 {
        return Ok(HandshakeResponse {
            capability: DEFAULT_CAPABILITY,
            user: String::new(),
            db: None,
            auth_plugin: String::new(),
        });
    }
    let capability = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let mut i = 32;
    let user = read_null_str(data, &mut i);
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
    let auth_plugin = if capability & CLIENT_PLUGIN_AUTH != 0 && i < data.len() {
        read_null_str(data, &mut i)
    } else {
        String::new()
    };
    Ok(HandshakeResponse {
        capability,
        user,
        db,
        auth_plugin,
    })
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

fn count_placeholders(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut n = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' || b == b'"' || b == b'`' {
            let q = b;
            i += 1;
            while i < bytes.len() && bytes[i] != q {
                i += 1;
            }
            i += 1;
            continue;
        }
        if b == b'?' {
            n += 1;
        }
        i += 1;
    }
    n
}

fn bind_placeholders(sql: &str, params: &[Datum]) -> Result<String> {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    let mut next = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' || b == b'"' || b == b'`' {
            let q = b;
            out.push(b as char);
            i += 1;
            while i < bytes.len() {
                out.push(bytes[i] as char);
                if bytes[i] == q {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if b == b'?' {
            let value = params
                .get(next)
                .ok_or_else(|| Error::new(ERR_UNKNOWN_ERROR, "Incorrect arguments to EXECUTE"))?;
            next += 1;
            out.push_str(&datum_sql_literal(value));
            i += 1;
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    Ok(out)
}

fn datum_sql_literal(value: &Datum) -> String {
    match value {
        Datum::Null => "NULL".into(),
        Datum::I64(v) => v.to_string(),
        Datum::U64(v) => v.to_string(),
        Datum::F64(v) => v.to_string(),
        Datum::Bytes(b) => {
            let s = String::from_utf8_lossy(b);
            format!("'{}'", s.replace('\'', "''"))
        }
        other => format!("'{}'", format!("{other:?}").replace('\'', "''")),
    }
}

/// Parse COM_STMT_EXECUTE binary parameters (null-bitmap + types + values).
fn parse_binary_params(payload: &[u8], num_params: u16) -> Result<Vec<Datum>> {
    if num_params == 0 {
        return Ok(Vec::new());
    }
    // stmt_id(4) + flags(1) + iteration_count(4) = 9
    let mut i = 9;
    let null_len = (num_params as usize).div_ceil(8);
    if payload.len() < i + null_len + 1 {
        return Err(Error::new(ERR_UNKNOWN_ERROR, "truncated execute params"));
    }
    let null_bitmap = &payload[i..i + null_len];
    i += null_len;
    let new_params = payload[i];
    i += 1;
    let mut types = Vec::with_capacity(num_params as usize);
    if new_params == 1 {
        for _ in 0..num_params {
            if i + 2 > payload.len() {
                return Err(Error::new(ERR_UNKNOWN_ERROR, "truncated param types"));
            }
            types.push(payload[i]);
            i += 2; // type + unsigned flag
        }
    } else {
        types.resize(num_params as usize, TYPE_VAR_STRING);
    }
    let mut out = Vec::with_capacity(num_params as usize);
    for (idx, ty) in types.into_iter().enumerate() {
        if null_bitmap[idx / 8] & (1 << (idx % 8)) != 0 {
            out.push(Datum::Null);
            continue;
        }
        out.push(read_binary_value(payload, &mut i, ty)?);
    }
    Ok(out)
}

fn read_binary_value(data: &[u8], i: &mut usize, ty: u8) -> Result<Datum> {
    match ty {
        TYPE_NULL => Ok(Datum::Null),
        TYPE_LONGLONG | 3 /* TYPE_LONG */ | 2 /* TYPE_SHORT */ | 1 /* TYPE_TINY */ => {
            let width = match ty {
                1 => 1,
                2 => 2,
                3 => 4,
                _ => 8,
            };
            if *i + width > data.len() {
                return Err(Error::new(ERR_UNKNOWN_ERROR, "truncated int param"));
            }
            let mut buf = [0u8; 8];
            buf[..width].copy_from_slice(&data[*i..*i + width]);
            *i += width;
            Ok(Datum::I64(i64::from_le_bytes(buf)))
        }
        TYPE_VAR_STRING | 0xfe /* TYPE_STRING */ | 15 /* TYPE_VARCHAR */ => {
            let n = read_lenenc(data, i)? as usize;
            if *i + n > data.len() {
                return Err(Error::new(ERR_UNKNOWN_ERROR, "truncated string param"));
            }
            let bytes = data[*i..*i + n].to_vec();
            *i += n;
            Ok(Datum::Bytes(bytes))
        }
        _ => {
 // Fallback: length-encoded string.
            let n = read_lenenc(data, i)? as usize;
            if *i + n > data.len() {
                return Err(Error::new(ERR_UNKNOWN_ERROR, "truncated param value"));
            }
            let bytes = data[*i..*i + n].to_vec();
            *i += n;
            Ok(Datum::Bytes(bytes))
        }
    }
}

fn binary_row_packet(row: &[Datum]) -> Vec<u8> {
    let mut buf = vec![0x00]; // binary row header
    let null_len = (row.len() + 2).div_ceil(8);
    let mut nulls = vec![0u8; null_len];
    for (i, value) in row.iter().enumerate() {
        if value.is_null() {
            let bit = i + 2;
            nulls[bit / 8] |= 1 << (bit % 8);
        }
    }
    buf.extend_from_slice(&nulls);
    for value in row {
        if value.is_null() {
            continue;
        }
        match value {
            Datum::I64(v) => buf.extend_from_slice(&v.to_le_bytes()),
            Datum::U64(v) => buf.extend_from_slice(&v.to_le_bytes()),
            Datum::F64(v) => buf.extend_from_slice(&v.to_le_bytes()),
            Datum::Bytes(b) => write_lenenc_str(&mut buf, b),
            other => write_lenenc_str(&mut buf, &other.as_bytes()),
        }
    }
    buf
}
