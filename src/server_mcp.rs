//! The optional embedded MCP server, binary-private to `ccal-server`.
//!
//! Pulled in from `src/bin/ccal-server.rs` via `#[path]` + `mod mcp;`, so —
//! exactly like the TUI's `app`/`ui`/`sync_client` — it lives in a binary,
//! never the library. AGENTS.md's hard rule holds: nothing here touches the
//! `automerge` crate (only `ccal::Store` / `ccal::models`), and all the
//! async/tokio lives outside the lib.
//!
//! ## Why it propagates live for free
//!
//! The server already holds one shared [`crate::Doc`] per docid:
//! `{ store: Mutex<Store>, changed: broadcast, dirty: Notify }`. A connected
//! TUI's `serve_peer` loop blocks on `changed.recv()` and re-flushes the
//! Automerge sync delta whenever *any* peer advances the doc. So an MCP tool
//! does precisely what `serve_peer` does after a received sync message:
//! mutate `doc.store`, then `dirty.notify_one()` (debounced save) +
//! `changed.send(())` (wake every peer). The assistant's edit reaches every
//! open TUI through the existing, proven sync path — no new sync code.
//!
//! ## Trust model
//!
//! Same as the WS sync path: a bearer-token gate at the HTTP layer, plaintext
//! behind Tailscale/TLS, single trusted operator. The full read+write tool
//! surface is the reason the whole MCP server is opt-in (`CCAL_MCP`).

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::Deserialize;
use serde_json::{json, Value};

use ccal::models::{Checkpoint, Note, NoteMeta, RestoreReport, Todo};
use ccal::Store;

use crate::Doc;

/// One MCP handler bound to a single shared document. Constructed fresh by
/// the `StreamableHttpService` factory per session; the `Arc<Doc>` inside is
/// the *same* replica every connected TUI peer syncs against.
#[derive(Clone)]
pub struct Ccal {
    doc: Arc<Doc>,
    // Read by the `#[tool_handler]`-generated `ServerHandler` impl, not by
    // hand — the dead-code lint can't see through the macro.
    #[allow(dead_code)]
    tool_router: ToolRouter<Ccal>,
}

// ---- param schemas (doc comments become the MCP tool arg descriptions) ----

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListNotesArgs {
    /// Optional folder path prefix, e.g. `["work","projects"]`. When given,
    /// only notes in that folder *or any subfolder* are returned. Omit for
    /// the whole corpus.
    #[serde(default)]
    pub folder: Option<Vec<String>>,
    /// Max notes to return (the page size). Omit for "no limit", but on a
    /// large corpus prefer paging — the body-free listing is still big.
    /// Results are ordered by folder path, then title, then id, so paging
    /// with `offset` is stable.
    #[serde(default)]
    pub limit: Option<usize>,
    /// How many notes to skip before the page (default 0). Pair with
    /// `limit` to walk the corpus.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Optional projection: which fields to include per note. `id` is
    /// always returned (it's the handle). Cheap fields: `title`, `folder`,
    /// `created`, `modified`, `private`. Body-derived fields (materialized
    /// for the returned page only, so page it): `body` (full markdown),
    /// `excerpt` (first ~200 chars, one line — ideal for triaging
    /// empty/junk/image-only notes), `body_len` (char count). Omit for all
    /// cheap fields (body-free, the default).
    #[serde(default)]
    pub fields: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveNotesArgs {
    /// The note ids to move (as returned by `list_notes`). Unknown ids are
    /// silently skipped.
    pub ids: Vec<String>,
    /// Destination folder path as an array. Empty = root. Created as
    /// needed. All listed notes land here.
    #[serde(default)]
    pub folder: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveFolderArgs {
    /// The folder subtree to move, e.g. `["gometro"]`. Every note whose
    /// path starts with this is moved. Must be non-empty.
    pub from: Vec<String>,
    /// The new path prefix, e.g. `["consulting","oldclients","gometro"]`.
    /// Can change depth/parent, not just rename in place. Empty = move the
    /// subtree to the root.
    #[serde(default)]
    pub to: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchNotesArgs {
    /// Case-insensitive text to find in note titles, folder paths and
    /// bodies. Returns matching notes (no bodies). A private note can only
    /// match on its title/folder, never its hidden body.
    pub query: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IdArgs {
    /// The note id (the app-owned UUID, as returned by `list_notes`).
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IdsArgs {
    /// The note ids to fetch (as returned by `list_notes`). Unknown ids are
    /// silently skipped. Order of the result follows this list.
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateNoteArgs {
    /// Folder path as an array, e.g. `["work","ideas"]`. Empty = root.
    /// Folders are implicit: filing a note here creates them.
    #[serde(default)]
    pub folder: Vec<String>,
    /// The note title.
    pub title: String,
    /// Optional initial markdown body.
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetTitleArgs {
    pub id: String,
    /// The new title.
    pub title: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetBodyArgs {
    pub id: String,
    /// The full new markdown body. Replaces the note body; the store
    /// splices only the changed region so concurrent edits still merge.
    pub body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveNoteArgs {
    pub id: String,
    /// The new folder path as an array. Empty = root. Creates folders as
    /// needed; the old folder vanishes if it ends up empty (folders are
    /// derived, not entities).
    #[serde(default)]
    pub folder: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RenameFolderArgs {
    /// The folder path to rename, e.g. `["work","old"]`. The last component
    /// is the one renamed; the whole subtree moves with it.
    pub path: Vec<String>,
    /// The new name for the final path component (a single segment, no
    /// separators).
    pub new_name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddTodoArgs {
    /// The todo text.
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetTodoTextArgs {
    pub id: String,
    /// The new todo text.
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SwapTodosArgs {
    /// One todo id.
    pub a: String,
    /// The other todo id. Their sort positions are exchanged.
    pub b: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateCheckpointArgs {
    /// Why you are checkpointing — describe the batch of changes you are
    /// about to make (for the "before" snapshot) or just made (for the
    /// "after" one). This is what a human reads to choose a restore point.
    pub reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CheckpointIdArgs {
    /// The checkpoint id, as returned by `create_checkpoint` /
    /// `list_checkpoints`.
    pub id: String,
}

// ---- JSON shaping --------------------------------------------------------

/// What the assistant sees instead of a private note's body.
const REDACTED: &str = "[REDACTED — this note is marked private; its \
contents are not available to assistants. You may still rename, move or \
delete it, but not read or edit its body.]";

/// Always honours privacy, so *every* path that returns a note is safe by
/// construction (no caller can forget to redact).
fn note_json(n: &Note) -> Value {
    json!({
        "id": n.id, "title": n.title, "folder": n.folder,
        "body": if n.private { REDACTED } else { n.body.as_str() },
        "created": n.created, "modified": n.modified,
        "private": n.private,
    })
}

fn meta_json(m: &NoteMeta) -> Value {
    json!({
        "id": m.id, "title": m.title, "folder": m.folder,
        "created": m.created, "modified": m.modified,
        "private": m.private,
    })
}

/// `meta_json` restricted to a projection. `id` is always kept; an empty /
/// absent field set means "all fields" (full `meta_json`).
/// Body-derived projection fields. Requesting any of these makes
/// `list_notes` materialize the note body for the *page only* — the
/// body-free default path is otherwise untouched.
const BODY_FIELDS: [&str; 3] = ["body", "excerpt", "body_len"];

/// Does this projection ask for anything that needs the note body?
fn wants_body(fields: Option<&[String]>) -> bool {
    fields.is_some_and(|fs| fs.iter().any(|f| BODY_FIELDS.contains(&f.as_str())))
}

/// First ~200 chars of `body`, whitespace collapsed to one line, ellipsed
/// if clipped — enough to triage (empty? image-only? a real note?) without
/// fetching the whole thing.
fn excerpt(body: &str) -> String {
    const MAX: usize = 200;
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

/// `meta_json` restricted to a projection. `id` is always kept; an empty /
/// absent field set means "all fields" (full `meta_json`, still body-free).
/// `note` is the materialized note, supplied only when the projection asks
/// for a [`BODY_FIELDS`] entry; privacy is honoured here too — a private
/// note's `body`/`excerpt` are redacted and its `body_len` is null (the
/// true length is itself withheld).
fn meta_json_proj(m: &NoteMeta, note: Option<&Note>, fields: Option<&[String]>) -> Value {
    let Some(fields) = fields else {
        return meta_json(m);
    };
    let mut o = serde_json::Map::new();
    o.insert("id".into(), json!(m.id));
    for f in fields {
        let (k, v): (&str, Value) = match f.as_str() {
            "title" => ("title", json!(m.title)),
            "folder" => ("folder", json!(m.folder)),
            "created" => ("created", json!(m.created)),
            "modified" => ("modified", json!(m.modified)),
            "private" => ("private", json!(m.private)),
            "body" => match note {
                Some(n) if n.private => ("body", json!(REDACTED)),
                Some(n) => ("body", json!(n.body)),
                None => continue,
            },
            "excerpt" => match note {
                Some(n) if n.private => ("excerpt", json!(REDACTED)),
                Some(n) => ("excerpt", json!(excerpt(&n.body))),
                None => continue,
            },
            "body_len" => match note {
                // The true length is itself withheld for a private note.
                Some(n) if n.private => ("body_len", Value::Null),
                Some(n) => ("body_len", json!(n.body.chars().count())),
                None => continue,
            },
            _ => continue,
        };
        o.insert(k.into(), v);
    }
    Value::Object(o)
}

/// A search hit: the note's meta plus an optional one-line context snippet
/// (`null` for title/folder-only or private matches — see
/// [`Store::search_notes_snippets`]).
fn search_hit_json(m: &NoteMeta, snippet: Option<&str>) -> Value {
    let mut v = meta_json(m);
    v.as_object_mut()
        .unwrap()
        .insert("snippet".into(), json!(snippet));
    v
}

fn todo_json(t: &Todo) -> Value {
    json!({ "id": t.id, "text": t.text, "order": t.order, "created": t.created })
}

fn checkpoint_json(c: &Checkpoint) -> Value {
    json!({ "id": c.id, "reason": c.reason, "created": c.created })
}

fn report_json(r: &RestoreReport) -> Value {
    json!({
        "notes_added": r.notes_added,
        "notes_deleted": r.notes_deleted,
        "notes_updated": r.notes_updated,
        "todos_added": r.todos_added,
        "todos_deleted": r.todos_deleted,
        "todos_updated": r.todos_updated,
    })
}

fn ok(v: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(v.to_string())]))
}

fn internal(e: anyhow::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn not_found(what: &str) -> McpError {
    McpError::invalid_params(format!("no such {what}"), None)
}

#[tool_router]
impl Ccal {
    pub fn new(doc: Arc<Doc>) -> Self {
        Self {
            doc,
            tool_router: Self::tool_router(),
        }
    }

    /// Briefly lock the doc, run `f`, then — only if it mutated — fire the
    /// same two signals `serve_peer` fires on a received change: debounce a
    /// save and wake every connected peer so the edit syncs live. The lock
    /// is held only across the in-memory mutation, never any IO.
    async fn mutate<T>(
        &self,
        f: impl FnOnce(&mut Store) -> anyhow::Result<T>,
    ) -> Result<T, McpError> {
        let out = {
            let mut st = self.doc.store.lock().await;
            f(&mut st).map_err(internal)?
        };
        self.doc.dirty.notify_one();
        let _ = self.doc.changed.send(());
        Ok(out)
    }

    #[tool(description = "List notes (id, title, folder, timestamps) without \
        their bodies. Optionally scoped to a folder and its subfolders, \
        paginated (`limit`/`offset`) and projected (`fields`). Results are \
        ordered by folder, then title, then id, so paging is stable. \
        Returns `{ notes, total, offset, limit }` where `total` is the \
        match count before paging. On a big corpus, call `list_folders` \
        first to see structure, then page this with a `folder` scope.")]
    async fn list_notes(
        &self,
        Parameters(args): Parameters<ListNotesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let st = self.doc.store.lock().await;
        let metas = st.note_metas();
        let prefix = args.folder.unwrap_or_default();
        let mut hits: Vec<NoteMeta> = metas
            .into_iter()
            .filter(|m| m.folder.len() >= prefix.len() && m.folder[..prefix.len()] == prefix[..])
            .collect();
        hits.sort_by(|a, b| {
            a.folder
                .cmp(&b.folder)
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.id.cmp(&b.id))
        });
        let total = hits.len();
        let offset = args.offset.unwrap_or(0).min(total);
        let end = match args.limit {
            Some(l) => (offset + l).min(total),
            None => total,
        };
        let fields = args.fields.as_deref();
        // Materialize bodies only for the page, and only if the projection
        // actually asked for a body-derived field.
        let need_body = wants_body(fields);
        let out: Vec<Value> = hits[offset..end]
            .iter()
            .map(|m| {
                let note = need_body.then(|| st.note(&m.id)).flatten();
                meta_json_proj(m, note.as_ref(), fields)
            })
            .collect();
        drop(st);
        ok(json!({
            "notes": out,
            "total": total,
            "offset": offset,
            "limit": args.limit,
        }))
    }

    #[tool(description = "The derived folder tree with note counts — the \
        cheap way to answer \"what folders do I have?\" without fetching \
        every note. Returns `{ folders: [{ path, direct, subtree }] }`: \
        `direct` = notes filed exactly there, `subtree` = notes there or \
        anywhere below. Parent folders with no direct notes are still \
        listed; root notes appear as path `[]`. Sorted by path.")]
    async fn list_folders(&self) -> Result<CallToolResult, McpError> {
        let tree = self.doc.store.lock().await.folder_tree();
        let out: Vec<Value> = tree
            .iter()
            .map(|(p, d, s)| json!({ "path": p, "direct": d, "subtree": s }))
            .collect();
        ok(json!({ "folders": out }))
    }

    #[tool(description = "Move many notes to one folder in a SINGLE \
        transaction (one change, one sync, one undo unit). Vastly cheaper \
        than calling move_note per id. Unknown ids are skipped. Returns \
        `{ moved: N }`.")]
    async fn move_notes(
        &self,
        Parameters(args): Parameters<MoveNotesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let n = self
            .mutate(|st| st.move_notes(&args.ids, &args.folder))
            .await?;
        ok(json!({ "moved": n }))
    }

    #[tool(description = "Move/rename a whole folder subtree in one \
        transaction: every note whose path starts with `from` has that \
        prefix replaced by `to`. Unlike rename_folder this can re-parent \
        and change depth (e.g. [\"gometro\"] -> \
        [\"consulting\",\"oldclients\",\"gometro\"]). This is the \
        intent-level tool for \"move these folders\" — no need to \
        enumerate ids. Returns `{ moved: N }`.")]
    async fn move_folder(
        &self,
        Parameters(args): Parameters<MoveFolderArgs>,
    ) -> Result<CallToolResult, McpError> {
        let n = self
            .mutate(|st| st.move_folder(&args.from, &args.to))
            .await?;
        ok(json!({ "moved": n }))
    }

    #[tool(description = "Full-text search across all notes (title, folder \
        path and body). Returns matching notes (no full body) each with a \
        one-line `snippet` of context around the body match — so you can \
        triage hits without a get_note per result. `snippet` is null when \
        the match was on title/folder only, or for a private note (which \
        only ever matches on title/folder — never its redacted body — so \
        this can't be used to probe hidden contents).")]
    async fn search_notes(
        &self,
        Parameters(args): Parameters<SearchNotesArgs>,
    ) -> Result<CallToolResult, McpError> {
        // `false`: never search private bodies (privacy boundary).
        let hits = self
            .doc
            .store
            .lock()
            .await
            .search_notes_snippets(&args.query, false);
        let out: Vec<Value> = hits
            .iter()
            .map(|(m, snip)| search_hit_json(m, snip.as_deref()))
            .collect();
        ok(json!({ "notes": out }))
    }

    #[tool(description = "Get one note's full contents, including its \
        markdown body.")]
    async fn get_note(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let note = self.doc.store.lock().await.note(&args.id);
        match note {
            Some(n) => ok(note_json(&n)),
            None => Err(not_found("note")),
        }
    }

    #[tool(description = "Get many notes' full contents in ONE call \
        (privacy-redacted like get_note; unknown ids skipped; result order \
        follows the request). Use this instead of N× get_note when \
        classifying or reviewing a batch — it's the read-side counterpart \
        of move_notes. Returns `{ notes: [...] }`.")]
    async fn get_notes(
        &self,
        Parameters(args): Parameters<IdsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let st = self.doc.store.lock().await;
        let out: Vec<Value> = args
            .ids
            .iter()
            .filter_map(|id| st.note(id).map(|n| note_json(&n)))
            .collect();
        drop(st);
        ok(json!({ "notes": out }))
    }

    #[tool(description = "Mark a note private (write-only, one-way). After \
        this its body is redacted to assistants and update_note_body is \
        refused — use it to lock down a note you discover contains secrets. \
        You CANNOT undo this or read the body afterwards; only the user can \
        clear `private`, in the TUI. Idempotent; safe on an already-private \
        note.")]
    async fn make_note_private(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.mutate(|st| {
            if st.note(&args.id).is_none() {
                return Err(anyhow::anyhow!("no such note"));
            }
            st.set_note_private(&args.id, true)
        })
        .await?;
        ok(json!({ "ok": true, "private": true }))
    }

    #[tool(description = "Create a note in the given folder (folders are \
        created implicitly). Returns the new note id.")]
    async fn create_note(
        &self,
        Parameters(args): Parameters<CreateNoteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = self
            .mutate(|st| {
                let id = st.create_note(&args.folder, &args.title)?;
                if let Some(body) = &args.body {
                    if !body.is_empty() {
                        st.set_note_body(&id, body)?;
                    }
                }
                Ok(id)
            })
            .await?;
        ok(json!({ "id": id }))
    }

    #[tool(description = "Rename a note (its title only).")]
    async fn set_note_title(
        &self,
        Parameters(args): Parameters<SetTitleArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.mutate(|st| st.set_note_title(&args.id, &args.title))
            .await?;
        ok(json!({ "ok": true }))
    }

    #[tool(description = "Replace a note's markdown body. Refused if the \
        note is marked private (you cannot rewrite a body you cannot see).")]
    async fn update_note_body(
        &self,
        Parameters(args): Parameters<SetBodyArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Check + mutate under one lock so privacy can't be toggled in a
        // race between the two (and privacy is TUI-only anyway).
        self.mutate(|st| {
            match st.note(&args.id) {
                Some(n) if n.private => {
                    return Err(anyhow::anyhow!(
                        "note is private; its body cannot be read or modified \
                         by assistants (you may still rename, move or delete it)"
                    ));
                }
                None => return Err(anyhow::anyhow!("no such note")),
                _ => {}
            }
            st.set_note_body(&args.id, &args.body)
        })
        .await?;
        ok(json!({ "ok": true }))
    }

    #[tool(description = "Move a note to a different folder (creating \
        folders as needed).")]
    async fn move_note(
        &self,
        Parameters(args): Parameters<MoveNoteArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.mutate(|st| st.set_note_folder(&args.id, &args.folder))
            .await?;
        ok(json!({ "ok": true }))
    }

    #[tool(description = "Recursively rename a folder: every note whose path \
        starts with it moves with it. Returns how many notes were updated.")]
    async fn rename_folder(
        &self,
        Parameters(args): Parameters<RenameFolderArgs>,
    ) -> Result<CallToolResult, McpError> {
        let n = self
            .mutate(|st| st.rename_folder(&args.path, &args.new_name))
            .await?;
        ok(json!({ "updated": n }))
    }

    #[tool(description = "Delete a note by id.")]
    async fn delete_note(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.mutate(|st| st.delete_note(&args.id)).await?;
        ok(json!({ "ok": true }))
    }

    #[tool(description = "List all todos in display order.")]
    async fn list_todos(&self) -> Result<CallToolResult, McpError> {
        let todos = self.doc.store.lock().await.todos();
        let out: Vec<Value> = todos.iter().map(todo_json).collect();
        ok(json!({ "todos": out }))
    }

    #[tool(description = "Add a todo to the end of the list. Returns its id.")]
    async fn add_todo(
        &self,
        Parameters(args): Parameters<AddTodoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = self.mutate(|st| st.add_todo(&args.text)).await?;
        ok(json!({ "id": id }))
    }

    #[tool(description = "Change a todo's text.")]
    async fn set_todo_text(
        &self,
        Parameters(args): Parameters<SetTodoTextArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.mutate(|st| st.set_todo_text(&args.id, &args.text))
            .await?;
        ok(json!({ "ok": true }))
    }

    #[tool(description = "Swap the list positions of two todos (reordering).")]
    async fn swap_todos(
        &self,
        Parameters(args): Parameters<SwapTodosArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.mutate(|st| st.swap_todo_order(&args.a, &args.b))
            .await?;
        ok(json!({ "ok": true }))
    }

    #[tool(description = "Delete a todo by id.")]
    async fn delete_todo(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.mutate(|st| st.delete_todo(&args.id)).await?;
        ok(json!({ "ok": true }))
    }

    #[tool(description = "Create a restore point capturing the CURRENT state \
        of all notes & todos. Call this BEFORE you start a batch of changes \
        (and again AFTER, to mark the finished state), with `reason` saying \
        what the batch is. Cheap — it copies nothing, just labels history. \
        Returns the checkpoint id.")]
    async fn create_checkpoint(
        &self,
        Parameters(args): Parameters<CreateCheckpointArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = self
            .mutate(|st| st.create_checkpoint(&args.reason))
            .await?;
        ok(json!({ "id": id }))
    }

    #[tool(description = "List all restore points, newest first.")]
    async fn list_checkpoints(&self) -> Result<CallToolResult, McpError> {
        let cps = self.doc.store.lock().await.checkpoints();
        let out: Vec<Value> = cps.iter().map(checkpoint_json).collect();
        ok(json!({ "checkpoints": out }))
    }

    #[tool(description = "Preview what restoring a checkpoint WOULD change, \
        without changing anything. Always do this before restore_checkpoint \
        so you (and the user) see the blast radius — restore reverts the \
        WHOLE corpus to that point, including unrelated edits made after it.")]
    async fn preview_restore(
        &self,
        Parameters(args): Parameters<CheckpointIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let rep = self
            .doc
            .store
            .lock()
            .await
            .preview_restore(&args.id)
            .map_err(internal)?;
        ok(report_json(&rep))
    }

    #[tool(description = "Restore the ENTIRE corpus (all notes & todos) to a \
        checkpoint's state. This reverts everything changed since then, not \
        just your edits. It is itself a normal forward change (history is \
        kept; you can still restore other checkpoints afterwards). Returns \
        the report of what changed.")]
    async fn restore_checkpoint(
        &self,
        Parameters(args): Parameters<CheckpointIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let rep = self
            .mutate(|st| st.restore_checkpoint(&args.id))
            .await?;
        ok(report_json(&rep))
    }
}

#[tool_handler]
impl ServerHandler for Ccal {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::default())
            .with_instructions(
                "ccal notes & todos. Notes are markdown documents in a \
                 derived folder tree (a note's `folder` is a path array; \
                 there is no folder entity — an empty folder simply ceases \
                 to exist). Todos are an ordered list. Use list_notes \
                 (body-free) to browse — add the `excerpt` projection \
                 field to triage many notes (empty/junk/image-only) in one \
                 call without fetching bodies; search_notes to find notes \
                 (each hit carries a context `snippet`); get_note for one \
                 note, or get_notes to pull a whole batch in a single call \
                 (prefer it over N× get_note). Edits made here sync live to \
                 any open ccal client.\n\n\
                 PRIVATE NOTES: a note with \"private\": true has its body \
                 redacted — you cannot read or edit it (update_note_body is \
                 refused). You CAN still rename, move and delete it, and \
                 call make_note_private to lock down a note you find holds \
                 secrets (one-way: only the user can un-private it, in the \
                 TUI). Never try to work around redaction; treat hidden \
                 contents as unknown.\n\n\
                 CHECKPOINT DISCIPLINE (important — edits are live and \
                 shared): whenever you are about to make a batch of \
                 changes, FIRST call create_checkpoint with a `reason` \
                 describing what you are about to do. Make the changes. \
                 Then call create_checkpoint AGAIN with a `reason` \
                 describing what you did. If something looks wrong, call \
                 list_checkpoints, then preview_restore to see the blast \
                 radius, then restore_checkpoint. Restore reverts the WHOLE \
                 corpus to that point (not just your edits), so prefer the \
                 nearest good checkpoint and always preview first."
                    .to_string(),
            )
    }
}
