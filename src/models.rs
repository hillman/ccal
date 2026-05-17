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
    /// User-set "do not show this to the LLM". Enforced only at the MCP
    /// boundary (redacted body, body-edit refused) — the TUI and sync keep
    /// the real content; this is LLM-scoping, not encryption at rest.
    pub private: bool,
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
    /// See [`Note::private`]. Carried in the body-free listing so the
    /// folder tree can show a lock and MCP `list_notes` can flag it.
    pub private: bool,
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

/// A named restore point. It does **not** copy the corpus — Automerge keeps
/// the whole history anyway, so a checkpoint is just a label plus the set of
/// document heads (hex change hashes) that content-address the state at
/// creation time. Restoring re-creates that state with a *forward* change
/// (a CRDT cannot rewind), so it syncs and saves like any other edit.
#[derive(Clone, Debug, Default)]
pub struct Checkpoint {
    pub id: String,
    /// Free-text note from whoever made it (the LLM is told to describe
    /// what it is about to do / has just done).
    pub reason: String,
    pub created: i64,
}

/// What a restore changed, so the caller (and the LLM) can see the blast
/// radius. A whole-corpus restore reverts *everything* to the checkpoint —
/// including edits made elsewhere after it — which these counts make visible.
#[derive(Clone, Debug, Default)]
pub struct RestoreReport {
    pub notes_added: usize,
    pub notes_deleted: usize,
    pub notes_updated: usize,
    pub todos_added: usize,
    pub todos_deleted: usize,
    pub todos_updated: usize,
}

/// One entry in the document's edit timeline (an Automerge change).
/// `ts` is epoch-ms (0 = unknown: pre-dates timestamped commits, or
/// genesis). `checkpoint` is the reason text when this change is a named
/// checkpoint's head, so the History view can show snapshots inline.
#[derive(Clone, Debug, Default)]
pub struct HistoryRow {
    pub hash: String,
    pub ts: i64,
    pub ops: usize,
    /// Short actor id (which replica/session made it).
    pub actor: String,
    pub checkpoint: Option<String>,
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
