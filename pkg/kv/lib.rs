#![forbid(unsafe_code)]
#![allow(clippy::module_inception)]

pub mod key;
pub mod kv;
pub mod mem;

pub use key::{IntHandle, Key};
pub use kv::{IsolationLevel, Storage, Transaction, Version};
pub use mem::MemStorage;
