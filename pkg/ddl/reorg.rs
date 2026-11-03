// TiDB `pkg/ddl/reorg.go` schema-state walk for ADD INDEX.
// Sync owner still applies states in-process; coprocessor workers /
// concurrent ingest stay deferred.
use sealdb_meta::{
    STATE_DELETE_ONLY, STATE_PUBLIC, STATE_WRITE_ONLY, STATE_WRITE_REORGANIZATION, SchemaState,
};

/// ADD INDEX reorganization: delete-only → write-only → write-reorg → public.
pub const INDEX_REORG_STATES: [SchemaState; 4] = [
    STATE_DELETE_ONLY,
    STATE_WRITE_ONLY,
    STATE_WRITE_REORGANIZATION,
    STATE_PUBLIC,
];

pub fn is_write_reorg(state: SchemaState) -> bool {
    state == STATE_WRITE_REORGANIZATION
}
