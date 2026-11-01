use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaState(pub u8);

pub const STATE_NONE: SchemaState = SchemaState(0);
pub const STATE_DELETE_ONLY: SchemaState = SchemaState(1);
pub const STATE_WRITE_ONLY: SchemaState = SchemaState(2);
pub const STATE_WRITE_REORGANIZATION: SchemaState = SchemaState(3);
pub const STATE_DELETE_REORGANIZATION: SchemaState = SchemaState(4);
pub const STATE_PUBLIC: SchemaState = SchemaState(5);
