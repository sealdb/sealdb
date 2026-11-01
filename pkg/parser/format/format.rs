#[derive(Default)]
pub struct RestoreCtx {
    buf: String,
}

impl RestoreCtx {
    pub fn new() -> Self {
        Self { buf: String::new() }
    }

    pub fn write_str(&mut self, s: &str) {
        self.buf.push_str(s);
    }

    pub fn into_string(self) -> String {
        self.buf
    }
}
