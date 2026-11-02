/// F22: MySQL packet I/O (TiDB `pkg/server/internal/packetio.go`).
/// Supports optional zlib compression (`CLIENT_COMPRESS`).
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use std::io::{Read, Write};

const HEADER_SIZE: usize = 4;
const COMPRESS_HEADER_SIZE: usize = 7;
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// Sync Read+Write used by PacketIO (plain TCP or rustls StreamOwned).
pub trait ConnIo: Read + Write + Send {}
impl<T: Read + Write + Send> ConnIo for T {}

pub struct PacketIO {
    stream: Box<dyn ConnIo>,
    sequence: u8,
    /// After handshake when both sides set `CLIENT_COMPRESS`.
    compressed: bool,
    /// Leftover uncompressed MySQL packets from a compressed frame.
    pending: Vec<u8>,
}

/// Discarding stand-in while PacketIO stream is upgraded to TLS.
struct NullIo;

impl Read for NullIo {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }
}

impl Write for NullIo {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl PacketIO {
    pub fn new(stream: impl ConnIo + 'static) -> Self {
        Self {
            stream: Box::new(stream),
            sequence: 0,
            compressed: false,
            pending: Vec::new(),
        }
    }

    pub fn sequence(&self) -> u8 {
        self.sequence
    }

    pub fn reset_sequence(&mut self) {
        self.sequence = 0;
    }

    pub fn set_sequence(&mut self, sequence: u8) {
        self.sequence = sequence;
    }

    pub fn set_compressed(&mut self, compressed: bool) {
        self.compressed = compressed;
    }

    pub fn compressed(&self) -> bool {
        self.compressed
    }

    /// Steal the underlying stream (leaves a NullIo placeholder).
    pub fn take_stream(&mut self) -> Box<dyn ConnIo> {
        std::mem::replace(&mut self.stream, Box::new(NullIo))
    }

    pub fn set_stream(&mut self, stream: Box<dyn ConnIo>) {
        self.stream = stream;
    }

    pub fn from_parts(stream: Box<dyn ConnIo>, sequence: u8) -> Self {
        Self {
            stream,
            sequence,
            compressed: false,
            pending: Vec::new(),
        }
    }

    pub fn read_packet(&mut self) -> Result<Vec<u8>> {
        if self.compressed {
            self.read_packet_compressed()
        } else {
            self.read_packet_plain()
        }
    }

    pub fn write_packet(&mut self, payload: &[u8]) -> Result<()> {
        if self.compressed {
            self.write_packet_compressed(payload)
        } else {
            self.write_packet_plain(payload)
        }
    }

    fn read_packet_plain(&mut self) -> Result<Vec<u8>> {
        let mut header = [0u8; HEADER_SIZE];
        self.stream.read_exact(&mut header).map_err(io_err)?;
        let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
        self.sequence = header[3].wrapping_add(1);
        if len > MAX_PAYLOAD {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "packet too large"));
        }
        let mut payload = vec![0u8; len];
        if len > 0 {
            self.stream.read_exact(&mut payload).map_err(io_err)?;
        }
        Ok(payload)
    }

    fn write_packet_plain(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "packet too large"));
        }
        let len = payload.len() as u32;
        let header = [
            len as u8,
            (len >> 8) as u8,
            (len >> 16) as u8,
            self.sequence,
        ];
        self.stream.write_all(&header).map_err(io_err)?;
        self.stream.write_all(payload).map_err(io_err)?;
        self.stream.flush().map_err(io_err)?;
        self.sequence = self.sequence.wrapping_add(1);
        Ok(())
    }

    fn read_packet_compressed(&mut self) -> Result<Vec<u8>> {
        if let Some(payload) = take_mysql_packet(&mut self.pending)? {
            return Ok(payload);
        }
        let mut header = [0u8; COMPRESS_HEADER_SIZE];
        self.stream.read_exact(&mut header).map_err(io_err)?;
        let comp_len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
        self.sequence = header[3].wrapping_add(1);
        let uncomp_len = u32::from_le_bytes([header[4], header[5], header[6], 0]) as usize;
        if comp_len > MAX_PAYLOAD || uncomp_len > MAX_PAYLOAD {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "compressed packet too large"));
        }
        let mut body = vec![0u8; comp_len];
        if comp_len > 0 {
            self.stream.read_exact(&mut body).map_err(io_err)?;
        }
        let plain = if uncomp_len == 0 {
            body
        } else {
            let mut decoder = ZlibDecoder::new(&body[..]);
            let mut out = Vec::with_capacity(uncomp_len);
            decoder.read_to_end(&mut out).map_err(io_err)?;
            out
        };
        self.pending.extend_from_slice(&plain);
        take_mysql_packet(&mut self.pending)?
            .ok_or_else(|| Error::new(ERR_UNKNOWN_ERROR, "empty compressed payload"))
    }

    fn write_packet_compressed(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "packet too large"));
        }
        // One MySQL packet per compressed frame; shared sequence id.
        let seq = self.sequence;
        let mut mysql = Vec::with_capacity(HEADER_SIZE + payload.len());
        let len = payload.len() as u32;
        mysql.extend_from_slice(&[len as u8, (len >> 8) as u8, (len >> 16) as u8, seq]);
        mysql.extend_from_slice(payload);

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&mysql).map_err(io_err)?;
        let compressed = encoder.finish().map_err(io_err)?;

        let (body, uncomp_len) = if compressed.len() >= mysql.len() {
            (mysql, 0u32)
        } else {
            (compressed, mysql.len() as u32)
        };
        let comp_len = body.len() as u32;
        let header = [
            comp_len as u8,
            (comp_len >> 8) as u8,
            (comp_len >> 16) as u8,
            seq,
            uncomp_len as u8,
            (uncomp_len >> 8) as u8,
            (uncomp_len >> 16) as u8,
        ];
        self.sequence = seq.wrapping_add(1);
        self.stream.write_all(&header).map_err(io_err)?;
        self.stream.write_all(&body).map_err(io_err)?;
        self.stream.flush().map_err(io_err)?;
        Ok(())
    }
}

fn take_mysql_packet(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    if buf.len() < HEADER_SIZE {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], 0]) as usize;
    if buf.len() < HEADER_SIZE + len {
        return Ok(None);
    }
    let payload = buf[HEADER_SIZE..HEADER_SIZE + len].to_vec();
    buf.drain(..HEADER_SIZE + len);
    Ok(Some(payload))
}

pub fn write_lenenc(buf: &mut Vec<u8>, n: u64) {
    if n < 251 {
        buf.push(n as u8);
    } else if n < 1 << 16 {
        buf.push(0xfc);
        buf.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n < 1 << 24 {
        buf.push(0xfd);
        buf.extend_from_slice(&(n as u32).to_le_bytes()[..3]);
    } else {
        buf.push(0xfe);
        buf.extend_from_slice(&n.to_le_bytes());
    }
}

pub fn write_lenenc_str(buf: &mut Vec<u8>, s: &[u8]) {
    write_lenenc(buf, s.len() as u64);
    buf.extend_from_slice(s);
}

pub fn write_null_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

pub fn read_lenenc(data: &[u8], i: &mut usize) -> Result<u64> {
    if *i >= data.len() {
        return Err(Error::new(ERR_UNKNOWN_ERROR, "truncated lenenc"));
    }
    let flag = data[*i];
    *i += 1;
    match flag {
        0xfb => Ok(0),
        0xfc => {
            if *i + 2 > data.len() {
                return Err(Error::new(ERR_UNKNOWN_ERROR, "truncated lenenc"));
            }
            let n = u16::from_le_bytes([data[*i], data[*i + 1]]) as u64;
            *i += 2;
            Ok(n)
        }
        0xfd => {
            if *i + 3 > data.len() {
                return Err(Error::new(ERR_UNKNOWN_ERROR, "truncated lenenc"));
            }
            let n = u32::from_le_bytes([data[*i], data[*i + 1], data[*i + 2], 0]) as u64;
            *i += 3;
            Ok(n)
        }
        0xfe => {
            if *i + 8 > data.len() {
                return Err(Error::new(ERR_UNKNOWN_ERROR, "truncated lenenc"));
            }
            let n = u64::from_le_bytes(data[*i..*i + 8].try_into().unwrap());
            *i += 8;
            Ok(n)
        }
        n => Ok(n as u64),
    }
}

pub fn read_null_str(data: &[u8], i: &mut usize) -> String {
    let start = *i;
    while *i < data.len() && data[*i] != 0 {
        *i += 1;
    }
    let s = String::from_utf8_lossy(&data[start..*i]).into_owned();
    if *i < data.len() {
        *i += 1;
    }
    s
}

fn io_err(err: std::io::Error) -> Error {
    Error::new(ERR_UNKNOWN_ERROR, format!("mysql packet io: {err}"))
}

#[cfg(test)]
mod compress_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Shared(Arc<Mutex<Vec<u8>>>, usize);

    impl Read for Shared {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let guard = self.0.lock().unwrap();
            let avail = &guard[self.1..];
            let n = avail.len().min(out.len());
            out[..n].copy_from_slice(&avail[..n]);
            self.1 += n;
            Ok(n)
        }
    }

    impl Write for Shared {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_compress_roundtrip() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut writer = PacketIO::new(Shared(Arc::clone(&buf), 0));
        writer.set_compressed(true);
        writer.write_packet(b"hello-compress").unwrap();
        let mut reader = PacketIO::new(Shared(buf, 0));
        reader.set_compressed(true);
        let got = reader.read_packet().unwrap();
        assert_eq!(got, b"hello-compress");
    }
}
