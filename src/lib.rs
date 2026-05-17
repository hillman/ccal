//! ccal core: domain models and the Automerge-backed store.
//!
//! The store is the single source of truth (an Automerge document persisted
//! as one binary blob). Everything — the TUI and the standalone Bear
//! importer — goes through [`store::Store`]; nothing else touches Automerge.

pub mod models;
pub mod store;

pub use store::{Store, SyncState};
