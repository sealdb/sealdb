//! TiDB `pkg/planner/cascades/memo/group_id_generator.go`.

#[derive(Clone, Debug, Default)]
pub struct GroupIdGenerator {
    next: u64,
}

impl GroupIdGenerator {
    pub fn next_id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
}
