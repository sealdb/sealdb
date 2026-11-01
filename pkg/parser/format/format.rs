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

    pub fn write_name(&mut self, name: &str) {
        self.buf.push('`');
        self.buf.push_str(name);
        self.buf.push('`');
    }

    pub fn into_string(self) -> String {
        self.buf
    }
}
