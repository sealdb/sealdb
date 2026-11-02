#![forbid(unsafe_code)]

pub mod privilege;
pub mod privileges;

pub use privilege::{Manager, PRIVILEGE_STORAGE_KEY};
pub use privileges::SKIP_WITH_GRANT_DEFAULT;
