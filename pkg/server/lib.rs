#![forbid(unsafe_code)]

pub mod conn;
pub mod internal;
pub mod server;

pub use conn::ClientConn;
pub use server::{Server, SslConfig, server_config_from_pem};

#[cfg(test)]
mod tests {
    use super::{Server, server_config_from_pem};
    use crate::conn::{IoAdapter, is_ssl_request};
    use crate::internal::packetio::{
        PacketIO, read_lenenc, write_lenenc, write_lenenc_str, write_null_str,
    };
    use sealdb_parser::mysql::{
        CLIENT_DEPRECATE_EOF, CLIENT_MULTI_RESULTS, CLIENT_MULTI_STATEMENTS, CLIENT_PLUGIN_AUTH,
        CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION, CLIENT_SSL, COM_PING, COM_QUERY, COM_QUIT,
        COM_STMT_CLOSE, COM_STMT_EXECUTE, COM_STMT_PREPARE, ERR_HEADER, OK_HEADER,
        SERVER_MORE_RESULTS_EXISTS, TYPE_LONGLONG,
    };
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    struct TestClient {
        pkt: PacketIO,
    }

    impl TestClient {
        fn connect(addr: std::net::SocketAddr) -> Self {
            let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
            Self {
                pkt: PacketIO::new(stream),
            }
        }

        fn handshake(&mut self) {
            let hello = self.pkt.read_packet().unwrap();
            assert_eq!(hello[0], 10);
            self.handshake_from_hello_done();
        }

        fn handshake_from_hello_done(&mut self) {
            let mut resp = Vec::new();
            let cap = CLIENT_PROTOCOL_41
                | CLIENT_SECURE_CONNECTION
                | CLIENT_PLUGIN_AUTH
                | CLIENT_DEPRECATE_EOF;
            resp.extend_from_slice(&cap.to_le_bytes());
            resp.extend_from_slice(&0x1000000u32.to_le_bytes());
            resp.push(45);
            resp.extend_from_slice(&[0u8; 23]);
            write_null_str(&mut resp, "root");
            resp.push(0);
            self.pkt.write_packet(&resp).unwrap();
            let ok = self.pkt.read_packet().unwrap();
            assert_eq!(ok[0], OK_HEADER);
        }

        fn query(&mut self, sql: &str) -> Result<Vec<Vec<String>>, String> {
            self.pkt.reset_sequence();
            let mut payload = vec![COM_QUERY];
            payload.extend_from_slice(sql.as_bytes());
            self.pkt.write_packet(&payload).unwrap();
            let first = self.pkt.read_packet().unwrap();
            if first.first() == Some(&ERR_HEADER) {
                return Err(String::from_utf8_lossy(&first[9..]).into_owned());
            }
            if first.first() == Some(&OK_HEADER) {
                return Ok(Vec::new());
            }
            let mut i = 0;
            let cols = read_lenenc(&first, &mut i).unwrap() as usize;
            for _ in 0..cols {
                let _ = self.pkt.read_packet().unwrap();
            }
            let mut rows = Vec::new();
            loop {
                let pkt = self.pkt.read_packet().unwrap();
                if pkt.first() == Some(&OK_HEADER) || pkt.first() == Some(&ERR_HEADER) {
                    if pkt.first() == Some(&ERR_HEADER) {
                        return Err(String::from_utf8_lossy(&pkt[9..]).into_owned());
                    }
                    break;
                }
                let mut off = 0;
                let mut row = Vec::new();
                for _ in 0..cols {
                    let n = read_lenenc(&pkt, &mut off).unwrap() as usize;
                    row.push(String::from_utf8_lossy(&pkt[off..off + n]).into_owned());
                    off += n;
                }
                rows.push(row);
            }
            Ok(rows)
        }

        fn ping(&mut self) {
            self.pkt.reset_sequence();
            self.pkt.write_packet(&[COM_PING]).unwrap();
            let ok = self.pkt.read_packet().unwrap();
            assert_eq!(ok[0], OK_HEADER);
        }

        fn quit(&mut self) {
            self.pkt.reset_sequence();
            let _ = self.pkt.write_packet(&[COM_QUIT]);
        }
    }

    fn start_server() -> std::net::SocketAddr {
        let server = Server::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        thread::spawn(move || {
            let _ = server.serve();
        });
        addr
    }

    fn start_tls_server() -> (std::net::SocketAddr, rcgen::Certificate) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_pem = certified.cert.pem();
        let key_pem = certified.key_pair.serialize_pem();
        let tls = server_config_from_pem(cert_pem.as_bytes(), key_pem.as_bytes()).unwrap();
        let server = Server::bind("127.0.0.1:0").unwrap().with_tls(tls);
        let addr = server.local_addr().unwrap();
        // Keep certificate for client trust store.
        let cert = certified.cert;
        thread::spawn(move || {
            let _ = server.serve();
        });
        (addr, cert)
    }

    #[test]
    fn test_com_query() {
        let addr = start_server();
        let mut client = TestClient::connect(addr);
        client.handshake();
        let rows = client.query("SELECT 1").unwrap();
        assert_eq!(rows, vec![vec!["1".to_string()]]);
        assert!(client.query("CREATE DATABASE db1").unwrap().is_empty());
        assert!(
            client
                .query("CREATE TABLE db1.t (id INT, name VARCHAR(20))")
                .unwrap()
                .is_empty()
        );
        assert!(
            client
                .query("INSERT INTO db1.t VALUES (1, 'a')")
                .unwrap()
                .is_empty()
        );
        let selected = client.query("SELECT * FROM db1.t").unwrap();
        assert_eq!(selected, vec![vec!["1".to_string(), "a".to_string()]]);
        let err = client.query("SELECT * FROM db1.missing").unwrap_err();
        assert!(err.contains("exist") || err.contains("Table"));
        client.ping();
        client.quit();
    }

    #[test]
    fn test_ssl_request_detect() {
        let mut req = Vec::new();
        let cap = CLIENT_PROTOCOL_41 | CLIENT_SSL | CLIENT_SECURE_CONNECTION;
        req.extend_from_slice(&cap.to_le_bytes());
        req.extend_from_slice(&0x1000000u32.to_le_bytes());
        req.push(45);
        req.extend_from_slice(&[0u8; 23]);
        assert_eq!(req.len(), 32);
        assert!(is_ssl_request(&req));
        assert!(!is_ssl_request(&req[..31]));
        req.extend_from_slice(b"root\0");
        assert!(!is_ssl_request(&req));
    }

    #[test]
    fn test_ssl_capability_advertised() {
        let (addr, _cert) = start_tls_server();
        let mut client = TestClient::connect(addr);
        let hello = client.pkt.read_packet().unwrap();
        assert_eq!(hello[0], 10);
        let mut i = 1;
        while i < hello.len() && hello[i] != 0 {
            i += 1;
        }
        i += 1; // null
        i += 4; // connection id
        i += 8; // salt part1
        i += 1; // filler
        let cap_lo = u16::from_le_bytes([hello[i], hello[i + 1]]);
        i += 2;
        i += 1; // charset
        i += 2; // status
        let cap_hi = u16::from_le_bytes([hello[i], hello[i + 1]]);
        let cap = u32::from(cap_lo) | (u32::from(cap_hi) << 16);
        assert_ne!(cap & CLIENT_SSL, 0, "server should advertise CLIENT_SSL");
        let mut resp = Vec::new();
        let client_cap = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_DEPRECATE_EOF;
        resp.extend_from_slice(&client_cap.to_le_bytes());
        resp.extend_from_slice(&0x1000000u32.to_le_bytes());
        resp.push(45);
        resp.extend_from_slice(&[0u8; 23]);
        write_null_str(&mut resp, "root");
        resp.push(0);
        client.pkt.write_packet(&resp).unwrap();
        let ok = client.pkt.read_packet().unwrap();
        assert_eq!(ok[0], OK_HEADER);
        let rows = client.query("SELECT 1").unwrap();
        assert_eq!(rows, vec![vec!["1".to_string()]]);
        client.quit();
    }

    #[test]
    fn test_ssl_request_without_cert_errors() {
        let addr = start_server();
        let mut client = TestClient::connect(addr);
        let hello = client.pkt.read_packet().unwrap();
        assert_eq!(hello[0], 10);
        let mut req = Vec::new();
        let cap = CLIENT_PROTOCOL_41 | CLIENT_SSL | CLIENT_SECURE_CONNECTION;
        req.extend_from_slice(&cap.to_le_bytes());
        req.extend_from_slice(&0x1000000u32.to_le_bytes());
        req.push(45);
        req.extend_from_slice(&[0u8; 23]);
        client.pkt.write_packet(&req).unwrap();
        let next = client.pkt.read_packet();
        assert!(next.is_err() || next.unwrap().first() == Some(&ERR_HEADER));
    }

    #[test]
    fn test_ssl_upgrade_roundtrip() {
        let (addr, cert) = start_tls_server();
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
        let mut pkt = PacketIO::new(stream);
        let hello = pkt.read_packet().unwrap();
        assert_eq!(hello[0], 10);

        let mut req = Vec::new();
        let cap = CLIENT_PROTOCOL_41
            | CLIENT_SSL
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_DEPRECATE_EOF;
        req.extend_from_slice(&cap.to_le_bytes());
        req.extend_from_slice(&0x1000000u32.to_le_bytes());
        req.push(45);
        req.extend_from_slice(&[0u8; 23]);
        pkt.write_packet(&req).unwrap();

        let plain = pkt.take_stream();
        let sequence = pkt.sequence();
        let mut root = rustls::RootCertStore::empty();
        root.add(cert.der().clone()).unwrap();
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(root)
            .with_no_client_auth();
        let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let conn = rustls::ClientConnection::new(Arc::new(client_config), server_name).unwrap();
        let mut tls = rustls::StreamOwned::new(conn, IoAdapter(plain));
        while tls.conn.is_handshaking() {
            tls.conn.complete_io(&mut tls.sock).unwrap();
        }
        let mut pkt = PacketIO::from_parts(Box::new(tls), sequence);

        let mut resp = Vec::new();
        resp.extend_from_slice(&cap.to_le_bytes());
        resp.extend_from_slice(&0x1000000u32.to_le_bytes());
        resp.push(45);
        resp.extend_from_slice(&[0u8; 23]);
        write_null_str(&mut resp, "root");
        resp.push(0);
        pkt.write_packet(&resp).unwrap();
        let ok = pkt.read_packet().unwrap();
        assert_eq!(ok[0], OK_HEADER);

        pkt.reset_sequence();
        let mut payload = vec![COM_QUERY];
        payload.extend_from_slice(b"SELECT 1");
        pkt.write_packet(&payload).unwrap();
        let first = pkt.read_packet().unwrap();
        let mut i = 0;
        let cols = read_lenenc(&first, &mut i).unwrap();
        assert_eq!(cols, 1);
    }

    #[test]
    fn test_com_stmt_prepare_execute() {
        let addr = start_server();
        let mut client = TestClient::connect(addr);
        client.handshake();
        assert!(client.query("CREATE DATABASE db1").unwrap().is_empty());
        assert!(
            client
                .query("CREATE TABLE db1.t (id INT, name VARCHAR(20))")
                .unwrap()
                .is_empty()
        );
        assert!(
            client
                .query("INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')")
                .unwrap()
                .is_empty()
        );

        client.pkt.reset_sequence();
        let mut prep = vec![COM_STMT_PREPARE];
        prep.extend_from_slice(b"SELECT name FROM db1.t WHERE id = ?");
        client.pkt.write_packet(&prep).unwrap();
        let ok = client.pkt.read_packet().unwrap();
        assert_eq!(ok[0], 0x00);
        let stmt_id = u32::from_le_bytes(ok[1..5].try_into().unwrap());
        let num_params = u16::from_le_bytes(ok[7..9].try_into().unwrap());
        assert_eq!(num_params, 1);
        // CLIENT_DEPRECATE_EOF: param column def only, no EOF.
        let _param_def = client.pkt.read_packet().unwrap();

        client.pkt.reset_sequence();
        let mut exec = vec![COM_STMT_EXECUTE];
        exec.extend_from_slice(&stmt_id.to_le_bytes());
        exec.push(0); // flags
        exec.extend_from_slice(&1u32.to_le_bytes()); // iteration
        exec.push(0); // null bitmap
        exec.push(1); // new-params-bound-flag
        exec.push(TYPE_LONGLONG);
        exec.push(0); // unsigned flag
        exec.extend_from_slice(&1i64.to_le_bytes());
        client.pkt.write_packet(&exec).unwrap();
        let first = client.pkt.read_packet().unwrap();
        assert_ne!(first.first(), Some(&ERR_HEADER), "{first:?}");
        let mut i = 0;
        let cols = read_lenenc(&first, &mut i).unwrap();
        assert_eq!(cols, 1);
        let _coldef = client.pkt.read_packet().unwrap();
        // CLIENT_DEPRECATE_EOF: no EOF between defs and rows.
        let row = client.pkt.read_packet().unwrap();
        assert_eq!(row.first(), Some(&0x00), "{row:?}"); // binary row
        let ok = client.pkt.read_packet().unwrap();
        assert_eq!(ok.first(), Some(&OK_HEADER));

        client.pkt.reset_sequence();
        let mut close = vec![COM_STMT_CLOSE];
        close.extend_from_slice(&stmt_id.to_le_bytes());
        client.pkt.write_packet(&close).unwrap();
        client.quit();
    }

    #[test]
    fn test_multi_resultset() {
        let addr = start_server();
        let mut client = TestClient::connect(addr);
        let hello = client.pkt.read_packet().unwrap();
        assert_eq!(hello[0], 10);
        let mut resp = Vec::new();
        let cap = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_DEPRECATE_EOF
            | CLIENT_MULTI_STATEMENTS
            | CLIENT_MULTI_RESULTS;
        resp.extend_from_slice(&cap.to_le_bytes());
        resp.extend_from_slice(&0x1000000u32.to_le_bytes());
        resp.push(45);
        resp.extend_from_slice(&[0u8; 23]);
        write_null_str(&mut resp, "root");
        resp.push(0);
        client.pkt.write_packet(&resp).unwrap();
        assert_eq!(client.pkt.read_packet().unwrap()[0], OK_HEADER);

        client.pkt.reset_sequence();
        let mut payload = vec![COM_QUERY];
        payload.extend_from_slice(b"SELECT 1; SELECT 2");
        client.pkt.write_packet(&payload).unwrap();

        // First resultset
        let first = client.pkt.read_packet().unwrap();
        let mut i = 0;
        assert_eq!(read_lenenc(&first, &mut i).unwrap(), 1);
        let _ = client.pkt.read_packet().unwrap(); // coldef
        let _ = client.pkt.read_packet().unwrap(); // row
        let eof1 = client.pkt.read_packet().unwrap();
        assert_eq!(eof1.first(), Some(&OK_HEADER));
        // OK: header + affected_rows(0) + last_insert_id(0) + status
        let status1 = u16::from_le_bytes([eof1[3], eof1[4]]);
        assert_ne!(status1 & SERVER_MORE_RESULTS_EXISTS, 0, "{eof1:?}");

        // Second resultset
        let second = client.pkt.read_packet().unwrap();
        let mut i = 0;
        assert_eq!(read_lenenc(&second, &mut i).unwrap(), 1);
        let _ = client.pkt.read_packet().unwrap();
        let _ = client.pkt.read_packet().unwrap();
        let eof2 = client.pkt.read_packet().unwrap();
        assert_eq!(eof2.first(), Some(&OK_HEADER));
        let status2 = u16::from_le_bytes([eof2[3], eof2[4]]);
        assert_eq!(status2 & SERVER_MORE_RESULTS_EXISTS, 0, "{eof2:?}");
        client.quit();
    }

    #[test]
    fn test_auth_plugin_access_denied() {
        let server = Server::bind_with_grant_table("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        thread::spawn(move || {
            let _ = server.serve();
        });
        let mut client = TestClient::connect(addr);
        let hello = client.pkt.read_packet().unwrap();
        assert_eq!(hello[0], 10);
        let mut resp = Vec::new();
        let cap = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_DEPRECATE_EOF;
        resp.extend_from_slice(&cap.to_le_bytes());
        resp.extend_from_slice(&0x1000000u32.to_le_bytes());
        resp.push(45);
        resp.extend_from_slice(&[0u8; 23]);
        write_null_str(&mut resp, "nobody");
        resp.push(0); // empty auth
        write_null_str(&mut resp, "mysql_native_password");
        client.pkt.write_packet(&resp).unwrap();
        let err = client.pkt.read_packet().unwrap();
        assert_eq!(err.first(), Some(&ERR_HEADER));
        assert!(String::from_utf8_lossy(&err).contains("Access denied"));
    }

    #[test]
    fn test_auth_plugin_root_ok() {
        let server = Server::bind_with_grant_table("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        thread::spawn(move || {
            let _ = server.serve();
        });
        let mut client = TestClient::connect(addr);
        let hello = client.pkt.read_packet().unwrap();
        assert_eq!(hello[0], 10);
        let mut resp = Vec::new();
        let cap = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_DEPRECATE_EOF;
        resp.extend_from_slice(&cap.to_le_bytes());
        resp.extend_from_slice(&0x1000000u32.to_le_bytes());
        resp.push(45);
        resp.extend_from_slice(&[0u8; 23]);
        write_null_str(&mut resp, "root");
        resp.push(0);
        write_null_str(&mut resp, "mysql_native_password");
        client.pkt.write_packet(&resp).unwrap();
        let ok = client.pkt.read_packet().unwrap();
        assert_eq!(ok.first(), Some(&OK_HEADER));
        let rows = client.query("SELECT 1").unwrap();
        assert_eq!(rows, vec![vec!["1".to_string()]]);
        client.quit();
    }

    #[test]
    fn test_compress_capability_advertised() {
        let addr = start_server();
        let mut client = TestClient::connect(addr);
        let hello = client.pkt.read_packet().unwrap();
        assert_eq!(hello[0], 10);
        let mut i = 1;
        while i < hello.len() && hello[i] != 0 {
            i += 1;
        }
        i += 1 + 4 + 8 + 1;
        let cap_lo = u16::from_le_bytes([hello[i], hello[i + 1]]);
        i += 2 + 1 + 2;
        let cap_hi = u16::from_le_bytes([hello[i], hello[i + 1]]);
        let cap = u32::from(cap_lo) | (u32::from(cap_hi) << 16);
        assert_ne!(
            cap & sealdb_parser::mysql::CLIENT_COMPRESS,
            0,
            "server should advertise CLIENT_COMPRESS"
        );
        // Clients that omit CLIENT_COMPRESS stay on plain packets.
        client.handshake_from_hello_done();
        let rows = client.query("SELECT 1").unwrap();
        assert_eq!(rows, vec![vec!["1".to_string()]]);
        client.quit();
    }

    #[test]
    fn test_lenenc_roundtrip() {
        let mut buf = Vec::new();
        write_lenenc(&mut buf, 10);
        write_lenenc_str(&mut buf, b"hi");
        let mut i = 0;
        assert_eq!(read_lenenc(&buf, &mut i).unwrap(), 10);
        assert_eq!(read_lenenc(&buf, &mut i).unwrap(), 2);
        assert_eq!(&buf[i..], b"hi");
    }
}
