use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_types::{Datum, EnumValue, MyDecimal, Time};
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub const SPILL_PARTITION_NUM: usize = 16;

static SEQ: AtomicU64 = AtomicU64::new(0);

const TAG_NULL: u8 = 0;
const TAG_I64: u8 = 1;
const TAG_U64: u8 = 2;
const TAG_F64: u8 = 3;
const TAG_BYTES: u8 = 4;
const TAG_DECIMAL: u8 = 5;
const TAG_TIME: u8 = 6;
const TAG_ENUM: u8 = 7;
const TAG_BIT: u8 = 8;

pub fn mem_row_limit() -> usize {
    std::env::var("SEALDB_SPILL_MEM_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(usize::MAX)
}

pub fn partition_index(keys: &[Datum], n: usize) -> usize {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for key in keys {
        format!("{key:?}").hash(&mut hasher);
    }
    (hasher.finish() as usize) % n.max(1)
}

pub struct DataInDiskByRows {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    reader: Option<BufReader<File>>,
    num_rows: usize,
}

impl DataInDiskByRows {
    pub fn new() -> Result<Self> {
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("sealdb-chunk-{}-{}.spill", std::process::id(), id));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(io_err)?;
        Ok(Self {
            path,
            writer: Some(BufWriter::new(file)),
            reader: None,
            num_rows: 0,
        })
    }

    pub fn add_row(&mut self, row: &[Datum]) -> Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| Error::new(ERR_UNKNOWN_ERROR, "spill file is not writable"))?;
        encode_row(writer, row)?;
        self.num_rows += 1;
        Ok(())
    }

    pub fn finish_write(&mut self) -> Result<()> {
        if let Some(mut writer) = self.writer.take() {
            writer.flush().map_err(io_err)?;
        }
        let file = File::open(&self.path).map_err(io_err)?;
        self.reader = Some(BufReader::new(file));
        Ok(())
    }

    pub fn next_row(&mut self) -> Result<Option<Vec<Datum>>> {
        let Some(reader) = self.reader.as_mut() else {
            return Ok(None);
        };
        decode_row(reader)
    }

    pub fn read_all(&mut self) -> Result<Vec<Vec<Datum>>> {
        let mut rows = Vec::with_capacity(self.num_rows);
        while let Some(row) = self.next_row()? {
            rows.push(row);
        }
        Ok(rows)
    }

    pub fn len(&self) -> usize {
        self.num_rows
    }

    pub fn is_empty(&self) -> bool {
        self.num_rows == 0
    }
}

impl Drop for DataInDiskByRows {
    fn drop(&mut self) {
        self.writer.take();
        self.reader.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn encode_row(w: &mut impl Write, row: &[Datum]) -> Result<()> {
    w.write_all(&(row.len() as u32).to_le_bytes())
        .map_err(io_err)?;
    for value in row {
        encode_datum(w, value)?;
    }
    Ok(())
}

fn encode_datum(w: &mut impl Write, value: &Datum) -> Result<()> {
    match value {
        Datum::Null => w.write_all(&[TAG_NULL]).map_err(io_err),
        Datum::I64(v) => {
            w.write_all(&[TAG_I64]).map_err(io_err)?;
            w.write_all(&v.to_le_bytes()).map_err(io_err)
        }
        Datum::U64(v) => {
            w.write_all(&[TAG_U64]).map_err(io_err)?;
            w.write_all(&v.to_le_bytes()).map_err(io_err)
        }
        Datum::F64(v) => {
            w.write_all(&[TAG_F64]).map_err(io_err)?;
            w.write_all(&v.to_le_bytes()).map_err(io_err)
        }
        Datum::Bytes(b) => {
            w.write_all(&[TAG_BYTES]).map_err(io_err)?;
            w.write_all(&(b.len() as u32).to_le_bytes())
                .map_err(io_err)?;
            w.write_all(b).map_err(io_err)
        }
        Datum::Bit(b) => {
            w.write_all(&[TAG_BIT]).map_err(io_err)?;
            w.write_all(&(b.len() as u32).to_le_bytes())
                .map_err(io_err)?;
            w.write_all(b).map_err(io_err)
        }
        Datum::Decimal(d) => {
            let bytes = d.to_string().into_bytes();
            w.write_all(&[TAG_DECIMAL]).map_err(io_err)?;
            w.write_all(&(bytes.len() as u32).to_le_bytes())
                .map_err(io_err)?;
            w.write_all(&bytes).map_err(io_err)
        }
        Datum::Time(t) => {
            w.write_all(&[TAG_TIME]).map_err(io_err)?;
            w.write_all(&t.to_packed_u64().to_le_bytes())
                .map_err(io_err)
        }
        Datum::Enum(e) => {
            w.write_all(&[TAG_ENUM]).map_err(io_err)?;
            w.write_all(&e.number.to_le_bytes()).map_err(io_err)?;
            w.write_all(&(e.label.len() as u32).to_le_bytes())
                .map_err(io_err)?;
            w.write_all(&e.label).map_err(io_err)
        }
    }
}

fn decode_row(r: &mut impl Read) -> Result<Option<Vec<Datum>>> {
    let Some(n_cols) = read_u32(r)? else {
        return Ok(None);
    };
    let mut row = Vec::with_capacity(n_cols as usize);
    for _ in 0..n_cols {
        row.push(decode_datum(r)?);
    }
    Ok(Some(row))
}

fn decode_datum(r: &mut impl Read) -> Result<Datum> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag).map_err(io_err)?;
    match tag[0] {
        TAG_NULL => Ok(Datum::Null),
        TAG_I64 => Ok(Datum::I64(read_fixed::<8>(r).map(i64::from_le_bytes)?)),
        TAG_U64 => Ok(Datum::U64(read_fixed::<8>(r).map(u64::from_le_bytes)?)),
        TAG_F64 => Ok(Datum::F64(read_fixed::<8>(r).map(f64::from_le_bytes)?)),
        TAG_BYTES => Ok(Datum::Bytes(read_len_bytes(r)?)),
        TAG_BIT => Ok(Datum::Bit(read_len_bytes(r)?)),
        TAG_DECIMAL => {
            let bytes = read_len_bytes(r)?;
            let text = String::from_utf8_lossy(&bytes);
            Ok(Datum::Decimal(MyDecimal::parse(&text)?))
        }
        TAG_TIME => Ok(Datum::Time(Time::from_packed_u64(
            read_fixed::<8>(r).map(u64::from_le_bytes)?,
        ))),
        TAG_ENUM => {
            let number = read_fixed::<8>(r).map(u64::from_le_bytes)?;
            let label = read_len_bytes(r)?;
            Ok(Datum::Enum(EnumValue { label, number }))
        }
        other => Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("unknown spill datum tag {other}"),
        )),
    }
}

fn read_u32(r: &mut impl Read) -> Result<Option<u32>> {
    let mut buf = [0u8; 4];
    match r.read_exact(&mut buf) {
        Ok(()) => Ok(Some(u32::from_le_bytes(buf))),
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => Ok(None),
        Err(err) => Err(io_err(err)),
    }
}

fn read_fixed<const N: usize>(r: &mut impl Read) -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    r.read_exact(&mut buf).map_err(io_err)?;
    Ok(buf)
}

fn read_len_bytes(r: &mut impl Read) -> Result<Vec<u8>> {
    let len = read_u32(r)?.ok_or_else(|| Error::new(ERR_UNKNOWN_ERROR, "truncated spill bytes"))?;
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).map_err(io_err)?;
    Ok(buf)
}

fn io_err(err: std::io::Error) -> Error {
    Error::new(ERR_UNKNOWN_ERROR, format!("spill io: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_row_in_disk_roundtrip() {
        let mut disk = DataInDiskByRows::new().unwrap();
        let rows = vec![
            vec![Datum::Null, Datum::I64(7)],
            vec![Datum::U64(3), Datum::F64(1.5)],
            vec![
                Datum::Bytes(b"hi".to_vec()),
                Datum::Decimal(MyDecimal::parse("12.50").unwrap()),
            ],
            vec![Datum::Time(Time::new(2024, 1, 2, 3, 4, 5)), Datum::I64(-1)],
        ];
        for row in &rows {
            disk.add_row(row).unwrap();
        }
        disk.finish_write().unwrap();
        assert_eq!(disk.read_all().unwrap(), rows);
    }

    #[test]
    fn test_partition_index_stable() {
        let key = vec![Datum::I64(42)];
        assert_eq!(partition_index(&key, 8), partition_index(&key, 8));
        assert!(partition_index(&key, 8) < 8);
    }
}
