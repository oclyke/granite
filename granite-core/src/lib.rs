#![forbid(unsafe_code)]

pub mod error;
pub mod header;
pub mod index;
pub mod merge;
pub mod meta;
pub mod pathenc;
pub mod plan;
pub mod scan;
pub mod shard;
pub mod verify;

mod extsort;
mod fsutil;
mod hash;
mod hex;
mod hints;
mod journal;
mod walk;

pub use error::{Error, Result};
pub use extsort::DEFAULT_BUDGET as DEFAULT_SORT_BUDGET;
pub use hex::encode as hex_encode;
pub use walk::default_jobs;
