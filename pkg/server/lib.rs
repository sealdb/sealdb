#![forbid(unsafe_code)]

pub mod conn;
pub mod internal;
pub mod server;

pub use conn::ClientConn;
pub use server::Server;

#[cfg(test)]
mod tests {
    use super::Server;
    use crate::internal::packetio::{
        PacketIO, read_lenenc, write_lenenc, write_lenenc_str, write_null_str,
    };
    use sealdb_parser::mysql::{
        CLIENT_DEPRECATE_EOF, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
        COM_PING, COM_QUERY, COM_QUIT, ERR_HEADER, OK_HEADER,
    };
    use std::net::TcpStream;
    use std::thread;
    use std::time::Duration;

    struct TestClient {
        pkt: PacketIO<TcpStream>,
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
