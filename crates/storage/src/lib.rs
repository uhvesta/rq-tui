//! SQLite persistence for rq-tui.
//!
//! The crate intentionally exposes one service and the records exchanged by
//! the application. Migration SQL and the SQLite connection stay private to
//! the implementation module.

mod storage;

pub use storage::{
    now, GitHubOperation, GitHubOperationKind, GitHubOperationPreparation, GitHubOperationState,
    PruneOperation, PruneTarget, RevQuestionSession, ReviewHistoryItem, SideSessionRecord, Storage,
};
