#![forbid(unsafe_code)]
#![allow(clippy::module_inception)]

pub mod key;
pub mod kv;

pub use key::{IntHandle, Key};
pub use kv::{IsolationLevel, Storage, Transaction, Version};
