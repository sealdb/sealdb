use super::column::Column;
use sealdb_types::Datum;

#[derive(Clone, Debug, Default)]
pub struct Chunk {
    columns: Vec<Column>,
    num_rows: usize,
}

impl Chunk {
    pub fn new(num_cols: usize, capacity: usize) -> Self {
        Self {
            columns: (0..num_cols)
                .map(|_| Column::with_capacity(capacity))
                .collect(),
            num_rows: 0,
        }
    }

    pub fn num_cols(&self) -> usize {
        self.columns.len()
    }

    pub fn num_rows(&self) -> usize {
        self.num_rows
    }

    pub fn append_row(&mut self, values: Vec<Datum>) {
        debug_assert_eq!(values.len(), self.columns.len());
        for (col, value) in self.columns.iter_mut().zip(values) {
            col.append_datum(value);
        }
        self.num_rows += 1;
    }

    pub fn get_row(&self, idx: usize) -> Vec<Datum> {
        self.columns
            .iter()
            .map(|c| c.get_datum(idx).cloned().unwrap_or(Datum::Null))
            .collect()
    }

    pub fn reset(&mut self) {
        for c in &mut self.columns {
            c.reset();
        }
        self.num_rows = 0;
    }
}
