#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct Key(pub Vec<u8>);

impl Key {
    pub fn from_raw(raw: Vec<u8>) -> Self {
        Self(raw)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    pub fn has_prefix(&self, prefix: &[u8]) -> bool {
        self.0.starts_with(prefix)
    }
}

impl From<Vec<u8>> for Key {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl From<Key> for Vec<u8> {
    fn from(value: Key) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntHandle(pub i64);

impl IntHandle {
    pub fn encoded(self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8);
        crate_encode_int(&mut buf, self.0);
        buf
    }
}

fn crate_encode_int(buf: &mut Vec<u8>, v: i64) {
    const SIGN_MASK: u64 = 0x8000_0000_0000_0000;
    buf.extend_from_slice(&((v as u64) ^ SIGN_MASK).to_be_bytes());
}
