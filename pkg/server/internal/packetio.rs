/// F22: MySQL packet I/O (TiDB `pkg/server/internal/packetio.go`).
/// Completeness wave: compression, max-allowed-packet config.
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use std::io::{Read, Write};

const HEADER_SIZE: usize = 4;
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// Sync Read+Write used by PacketIO (plain TCP or rustls StreamOwned).
pub trait ConnIo: Read + Write + Send {}
impl<T: Read + Write + Send> ConnIo for T {}

pub struct PacketIO {
    stream: Box<dyn ConnIo>,
    sequence: u8,
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

    /// Steal the underlying stream (leaves a NullIo placeholder).
    pub fn take_stream(&mut self) -> Box<dyn ConnIo> {
        std::mem::replace(&mut self.stream, Box::new(NullIo))
    }

    pub fn set_stream(&mut self, stream: Box<dyn ConnIo>) {
        self.stream = stream;
    }

    pub fn from_parts(stream: Box<dyn ConnIo>, sequence: u8) -> Self {
        Self { stream, sequence }
    }

    pub fn read_packet(&mut self) -> Result<Vec<u8>> {
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

    pub fn write_packet(&mut self, payload: &[u8]) -> Result<()> {
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
