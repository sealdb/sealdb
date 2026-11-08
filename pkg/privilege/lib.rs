#![forbid(unsafe_code)]

pub mod privilege;
pub mod privileges;

pub use privilege::{Manager, PRIVILEGE_STORAGE_KEY, set_thread_user, thread_user};
pub use privileges::SKIP_WITH_GRANT_DEFAULT;
