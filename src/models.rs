//! Pure domain types. No Automerge, no I/O — just the shapes the rest of the
//! app talks in. Identity is our own (UUID v4); nothing here knows or cares
//! that the data originally came from Bear or anywhere else.

use uuid::Uuid;

/// A stable, app-owned note identifier.
pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Debug, Default)]
pub struct Note {
    pub id: String,
    pub title: String,
    /// Folder path as data, e.g. `["work", "projects"]`. Empty = root.
    pub folder: Vec<String>,
    pub body: String,
    pub created: i64,
    pub modified: i64,
}

/// Note without its body — for listing/browsing. Reconstructing a note's
/// `Text` body is comparatively expensive, so the folder tree uses this.
#[derive(Clone, Debug, Default)]
pub struct NoteMeta {
    pub id: String,
    pub title: String,
    pub folder: Vec<String>,
    pub created: i64,
    pub modified: i64,
}

/// One note to bulk-import. Identity is assigned by the store (our own
/// UUID); external systems' keys are never reused.
#[derive(Clone, Debug, Default)]
pub struct NoteInput {
    pub folder: Vec<String>,
    pub title: String,
    pub body: String,
    pub created: i64,
    pub modified: i64,
}

#[derive(Clone, Debug, Default)]
pub struct Todo {
    pub id: String,
    pub text: String,
    /// Sort key. Fractional so a single reorder touches one field and
    /// merges cleanly across replicas; ties broken by `id`.
    pub order: f64,
    pub created: i64,
}
