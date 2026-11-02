use super::common::{CODEC_VER, ROW_FLAG_CHECKSUM, ROW_FLAG_LARGE};

/// Parsed CodecVer 128 row: sorted col ids, not-null offsets, and raw data bytes.
/// Layout matches TiDB `pkg/util/rowcodec/row.go`.
pub struct Row<'a> {
    pub flags: u8,
    pub num_not_null: u16,
    pub num_null: u16,
    /// Not-null ids first, then null ids; both runs sorted by id.
    pub col_ids: Vec<u32>,
    /// Exclusive end offset of each not-null column in `data`.
    pub offsets: Vec<u32>,
    pub data: &'a [u8],
}

impl<'a> Row<'a> {
    /// Parse a v128 row. Returns `None` if the header is truncated or version mismatches.
    pub fn from_bytes(row_data: &'a [u8]) -> Option<Self> {
        if row_data.len() < 6 || row_data[0] != CODEC_VER {
            return None;
        }
        let flags = row_data[1];
        let num_not_null = u16::from_le_bytes(row_data[2..4].try_into().ok()?);
        let num_null = u16::from_le_bytes(row_data[4..6].try_into().ok()?);
        let large = flags & ROW_FLAG_LARGE != 0;
        let mut cursor = 6usize;
        let n_ids = (num_not_null as usize) + (num_null as usize);
        let mut col_ids = Vec::with_capacity(n_ids);
        if large {
            let ids_len = n_ids.checked_mul(4)?;
            if cursor + ids_len > row_data.len() {
                return None;
            }
            for chunk in row_data[cursor..cursor + ids_len].as_chunks::<4>().0 {
                col_ids.push(u32::from_le_bytes(*chunk));
            }
            cursor += ids_len;
        } else {
            if cursor + n_ids > row_data.len() {
                return None;
            }
            col_ids.extend(
                row_data[cursor..cursor + n_ids]
                    .iter()
                    .map(|b| u32::from(*b)),
            );
            cursor += n_ids;
        }
        let mut offsets = Vec::with_capacity(num_not_null as usize);
        let last_offset = if large {
            let off_len = (num_not_null as usize).checked_mul(4)?;
            if cursor + off_len > row_data.len() {
                return None;
            }
            for chunk in row_data[cursor..cursor + off_len].as_chunks::<4>().0 {
                offsets.push(u32::from_le_bytes(*chunk));
            }
            cursor += off_len;
            offsets.last().copied().unwrap_or(0) as usize
        } else {
            let off_len = (num_not_null as usize).checked_mul(2)?;
            if cursor + off_len > row_data.len() {
                return None;
            }
            for chunk in row_data[cursor..cursor + off_len].as_chunks::<2>().0 {
                offsets.push(u32::from(u16::from_le_bytes(*chunk)));
            }
            cursor += off_len;
            offsets.last().copied().unwrap_or(0) as usize
        };
        if cursor + last_offset > row_data.len() {
            return None;
        }
        let data = &row_data[cursor..cursor + last_offset];
        Some(Self {
            flags,
            num_not_null,
            num_null,
            col_ids,
            offsets,
            data,
        })
    }

    /// Byte slice for the `not_null_idx`-th not-null column.
    pub fn col_data(&self, not_null_idx: usize) -> Option<&'a [u8]> {
        let end = *self.offsets.get(not_null_idx)? as usize;
        let start = if not_null_idx == 0 {
            0
        } else {
            self.offsets[not_null_idx - 1] as usize
        };
        self.data.get(start..end)
    }

    /// Binary-search `col_id` in the sorted not-null then null id lists.
    pub fn find_col_id(&self, col_id: i64) -> FindCol {
        let id = col_id as u32;
        let not_null = &self.col_ids[..self.num_not_null as usize];
        if let Ok(idx) = not_null.binary_search(&id) {
            return FindCol::NotNull(idx);
        }
        let nulls = &self.col_ids[self.num_not_null as usize..];
        if nulls.binary_search(&id).is_ok() {
            return FindCol::Null;
        }
        FindCol::NotFound
    }
}

/// Result of looking up a column id in a v128 row.
pub enum FindCol {
    /// Present and not null; index into `offsets` / not-null `col_ids`.
    NotNull(usize),
    Null,
    NotFound,
}

/// Serialize a v128 row. Checksum flag is cleared; checksum bytes are not written.
pub fn to_bytes(
    flags: u8,
    not_null_ids: &[u32],
    null_ids: &[u32],
    offsets: &[u32],
    data: &[u8],
) -> Vec<u8> {
    let large = flags & ROW_FLAG_LARGE != 0;
    let mut buf = Vec::with_capacity(6 + data.len() + not_null_ids.len() * 8);
    buf.push(CODEC_VER);
    buf.push(flags & !ROW_FLAG_CHECKSUM);
    buf.extend_from_slice(&(not_null_ids.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(null_ids.len() as u16).to_le_bytes());
    if large {
        for id in not_null_ids.iter().chain(null_ids) {
            buf.extend_from_slice(&id.to_le_bytes());
        }
        for off in offsets {
            buf.extend_from_slice(&off.to_le_bytes());
        }
    } else {
        for id in not_null_ids.iter().chain(null_ids) {
            buf.push(*id as u8);
        }
        for off in offsets {
            buf.extend_from_slice(&(*off as u16).to_le_bytes());
        }
    }
    buf.extend_from_slice(data);
    buf
}
