//! SQLite persistence for rq-tui.
//!
//! The crate intentionally exposes one service and the records exchanged by
//! the application. Migration SQL and the SQLite connection stay private to
//! the implementation module.

mod storage;

pub use storage::{
    now, PruneOperation, PruneTarget, ReviewHistoryItem, SideSessionRecord, Storage,
};
