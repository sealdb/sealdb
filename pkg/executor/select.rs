use crate::internal::exec::Executor;
use sealdb_errno::Result;
use sealdb_util::chunk::Chunk;

pub struct TableDualExec {
    opened: bool,
    emitted: bool,
}

impl TableDualExec {
    pub fn new() -> Self {
        Self {
            opened: false,
            emitted: false,
        }
    }
}

impl Default for TableDualExec {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor for TableDualExec {
    fn open(&mut self) -> Result<()> {
        self.opened = true;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if !self.opened || self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        Ok(Some(Chunk::new(0, 1)))
    }

    fn close(&mut self) -> Result<()> {
        self.opened = false;
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &[]
    }
}
