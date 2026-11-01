use sealdb_types::Datum;

#[derive(Clone, Debug, Default)]
pub struct Column {
    data: Vec<Datum>,
}

impl Column {
    pub fn new() -> Self {
        Self { data: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            data: Vec::with_capacity(cap),
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn append_datum(&mut self, d: Datum) {
        self.data.push(d);
    }

    pub fn get_datum(&self, row: usize) -> Option<&Datum> {
        self.data.get(row)
    }

    pub fn reset(&mut self) {
        self.data.clear();
    }
}
