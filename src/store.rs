//! The single source of truth: one Automerge document, persisted as one
//! binary blob in the data dir. This module is the *only* place that knows
//! Automerge exists; callers work entirely in [`crate::models`] terms.
//!
//! Built on the low-level `Automerge` API with explicit transactions: each
//! interactive edit is its own small transaction, and bulk import is a
//! single transaction over all notes (doing it per-note through the
//! convenience `AutoCommit` wrapper is pathologically slow at scale).
//!
//! `body` is an Automerge `List<Str>` — one element per *line*. Concurrent
//! edits to *different* lines of the same note merge; two edits to the *same*
//! line resolve last-writer-wins. This is deliberately coarser than a
//! per-character `Text` CRDT: char-level cost O(every character in the
//! corpus) at load (a sequence CRDT stores one op per character, so loading
//! ~800 KB of notes meant materializing ~800 K ops, ~1.2 s in WASM), whereas
//! line-level is O(lines) — ~30× fewer ops, ~24× faster load — while still
//! merging the realistic case (different lines changed on different devices).
//! Interactive edits splice only the changed line range so untouched lines
//! keep their identity and still merge. (Schema bumped 1→2 for this; see
//! `Store::migrate_v1_in_place`.)
//!
//! Document layout (ROOT map):
//! - `schema`: Int
//! - `notes`:  Map  id -> { title:Str, folder:List<Str>, body:List<Str>,
//!                           created:Int, modified:Int }
//! - `todos`:  Map  id -> { text:Str, order:F64, created:Int,
//!                           tags:List<Str>? (absent = none) }
//! - `cal/<id>`: Map { url:Str, name:Str, created:Int }
//!
//! Each calendar *subscription* is its own `cal/<id>` ROOT key (not a child
//! of a shared `calendars` map) so it converges with no genesis change.
//! Fetched events are NOT stored in the doc — they are a non-synced local
//! cache (see the TUI's `cal_sync`).

use anyhow::{anyhow, Context, Result};
use automerge::sync::{Message as AmSyncMessage, SyncDoc};
use automerge::transaction::{CommitOptions, Transactable, Transaction};
use automerge::{ActorId, Automerge, ObjId, ObjType, Prop, ReadDoc, ScalarValue, Value, ROOT};
use std::path::{Path, PathBuf};

/// Per-peer sync state for the Automerge sync protocol. Opaque to callers;
/// create one with [`SyncState::default`] per connected peer and keep it for
/// the life of that connection.
pub use automerge::sync::State as SyncState;

use automerge::ChangeHash;
use std::str::FromStr;

use crate::models::{
    new_id, now_ms, Calendar, Checkpoint, HistoryRow, Note, NoteInput, NoteMeta, RestoreReport,
    Todo,
};

const SCHEMA: i64 = 2;

/// Prefix for the per-calendar ROOT keys (`cal/<uuid>`). See [`Store`] doc:
/// each subscription is its own `ROOT` entry, not a child of a shared
/// container, so it converges with no genesis change.
const CAL_PREFIX: &str = "cal/";

/// Prefix for global bookmark ROOT keys (`mark/<char>` -> note id). Like
/// `cal/`, each mark is its own scalar `ROOT` entry, not a child of a
/// shared container — so it converges with no genesis change AND is safe
/// for the multi-writer TUI clients (the lazy single-container trick used
/// by `checkpoints` is only sound for the single-writer server). A
/// concurrent set of the same key resolves deterministically to one note,
/// which is acceptable last-writer-wins semantics for a bookmark.
const MARK_PREFIX: &str = "mark/";

/// Wall-clock seconds — Automerge change timestamps are unix *seconds*.
fn now_secs() -> i64 {
    now_ms() / 1000
}

/// Commit an interactive transaction, stamping it with the current time so
/// the History view has a real timeline. The timestamp is purely advisory
/// (Automerge does not use it in conflict resolution), so this does NOT
/// affect convergence. NOTE: `genesis_doc` deliberately commits with
/// `with_time(0)` instead — its change must stay byte-identical across
/// replicas, so it must never use this.
fn commit(tx: Transaction<'_>) {
    tx.commit_with(CommitOptions::default().with_time(now_secs()));
}

pub struct Store {
    doc: Automerge,
    path: PathBuf,
    notes: ObjId,
    todos: ObjId,
    /// `ROOT["checkpoints"]`, resolved only if it already exists. Unlike
    /// `notes`/`todos` (seeded by the shared genesis so every replica
    /// agrees on the ObjId), `checkpoints` is **not** in genesis — adding
    /// it there would change the genesis bytes and desync every existing
    /// replica. Instead it is created lazily by the FIRST checkpoint write.
    /// That is safe specifically because there is exactly one writer: the
    /// single always-on `ccal-server` (the only place the MCP server runs).
    /// So the genesis-class "two peers each seed their own ROOT map and
    /// never converge" hazard cannot arise here. If checkpoints ever gain a
    /// second independent writer, this assumption must be revisited.
    checkpoints: Option<ObjId>,
}

fn data_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "ccal")
        .context("could not determine a data directory")?;
    let dir = dirs.data_dir().to_path_buf();
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("ccal.automerge"))
}

/// The canonical empty document every replica starts from.
///
/// A fixed actor id and a fixed commit timestamp make the genesis change
/// byte-for-byte identical on every machine, so all replicas share one
/// common ancestor and resolve `notes`/`todos` to the *same* object id.
/// This is the precondition for the Automerge sync protocol to converge —
/// without it, independently created root maps conflict permanently.
fn genesis_doc() -> Automerge {
    let actor: ActorId = "cca100000000cca100000000"
        .parse()
        .expect("valid genesis actor id");
    let mut doc = Automerge::new().with_actor(actor);
    let mut tx = doc.transaction();
    tx.put(ROOT, "schema", SCHEMA).expect("genesis schema");
    tx.put_object(ROOT, "notes", ObjType::Map)
        .expect("genesis notes map");
    tx.put_object(ROOT, "todos", ObjType::Map)
        .expect("genesis todos map");
    tx.commit_with(CommitOptions::default().with_time(0));
    doc
}

impl Store {
    /// Open the store from the default ccal data directory, creating an empty
    /// document if absent.
    pub fn open() -> Result<Self> {
        Self::open_at(data_path()?)
    }

    /// The default on-disk replica path (`<data dir>/ccal.automerge`) — so a
    /// caller can run [`Store::needs_migration`] / [`Store::migrate_v1_in_place`]
    /// against it before [`Store::open`].
    pub fn default_path() -> Result<PathBuf> {
        data_path()
    }

    /// Open the store from an explicit path (used by the sync server, which
    /// keeps its replica outside the interactive client's data dir),
    /// creating an empty document if absent.
    pub fn open_at(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut doc = if path.exists() {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            Automerge::load(&bytes).context("parsing Automerge document")?
        } else {
            // Fresh replica: start from the shared genesis, NOT a blank
            // doc. Two blank docs would each `put_object` their own
            // `notes`/`todos` maps with no common ancestor; after sync the
            // root keys conflict and writes land in different objects that
            // never converge. Genesis gives every replica (client and
            // server alike) one byte-identical ancestor.
            genesis_doc()
        };

        // The genesis actor signs *only* the genesis change. Every replica
        // must make its own subsequent edits under a distinct actor, or two
        // replicas both writing as the genesis actor collide ("duplicate
        // seq"). A fresh actor per open is fine for Automerge.
        doc.set_actor(ActorId::random());

        let mut tx = doc.transaction();
        if tx.get(ROOT, "schema")?.is_none() {
            tx.put(ROOT, "schema", SCHEMA)?;
        }
        let notes = ensure_map(&mut tx, "notes")?;
        let todos = ensure_map(&mut tx, "todos")?;
        commit(tx);

        // Resolve `checkpoints` ONLY if a prior checkpoint already created
        // it — never create it here. Creating on open would emit a
        // map-creation change from every replica, reintroducing exactly the
        // concurrent-seed divergence genesis exists to prevent.
        let checkpoints = match doc.get(ROOT, "checkpoints") {
            Ok(Some((Value::Object(_), id))) => Some(id),
            _ => None,
        };

        Ok(Self { doc, path, notes, todos, checkpoints })
    }

    /// Persist the document atomically (temp file + rename).
    pub fn save(&mut self) -> Result<()> {
        let bytes = self.doc.save();
        let tmp = self.path.with_extension("automerge.tmp");
        std::fs::write(&tmp, &bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).context("replacing store file")?;
        Ok(())
    }

    // ---- Migration -----------------------------------------------------

    /// The on-disk `schema` of the document at `path`, or `None` if the file
    /// is absent. Loads the whole document (the only way to read a value), so
    /// it is as costly as an `open` — fine for a once-per-launch check.
    pub fn file_schema(path: impl AsRef<Path>) -> Result<Option<i64>> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let doc = Automerge::load(&bytes).context("parsing Automerge document")?;
        Ok(Some(read_schema(&doc)))
    }

    /// `true` if the document at `path` predates the current schema and a
    /// client should reset (discard + re-sync) rather than push its old
    /// history back onto a migrated server. `false` for an absent or
    /// already-current file.
    pub fn needs_migration(path: impl AsRef<Path>) -> Result<bool> {
        Ok(Self::file_schema(path)?.is_some_and(|s| s < SCHEMA))
    }

    /// Re-genesis a pre-line-body replica in place.
    ///
    /// A schema-1 document stores each `body` as a per-character `Text`, so it
    /// carries one op per character ever typed — the load-time cost this whole
    /// change exists to remove. Automerge never forgets ops, so the only way
    /// to shed them is to mint a *fresh* document: this builds a new genesis
    /// doc with `body` as a `List<Str>` of lines, copying every note, todo,
    /// calendar subscription and mark across **by id** (so marks and external
    /// references stay valid). Edit history and checkpoints are intentionally
    /// dropped — they are exactly the op history we are discarding.
    ///
    /// The original file is kept as `<name>.v1.bak`. Returns `Ok(true)` if a
    /// migration ran, `Ok(false)` if the file is absent or already current
    /// (idempotent). Server-authoritative: clients should `needs_migration` →
    /// discard + re-sync instead of migrating independently, so this change's
    /// non-deterministic commit never has to converge with another replica's.
    pub fn migrate_v1_in_place(path: impl AsRef<Path>) -> Result<bool> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(false);
        }
        // Load once and gate on the in-memory schema (loading an old replica
        // is the costly part — don't do it twice).
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let old = Automerge::load(&bytes).context("parsing v1 document")?;
        if read_schema(&old) >= SCHEMA {
            return Ok(false);
        }
        let mut doc = genesis_doc();
        doc.set_actor(ActorId::random());
        let notes = child(&doc, &ROOT, "notes").context("genesis notes")?;
        let todos = child(&doc, &ROOT, "todos").context("genesis todos")?;

        let mut tx = doc.transaction();
        if let Some(on) = child(&old, &ROOT, "notes") {
            for id in old.keys(&on) {
                let Some(o) = child(&old, &on, &id) else { continue };
                // The OLD body is a `Text`; read it as text, store as lines.
                let body = child(&old, &o, "body")
                    .and_then(|b| old.text(&b).ok())
                    .unwrap_or_default();
                let obj = tx.put_object(&notes, id.as_str(), ObjType::Map)?;
                tx.put(&obj, "title", get_str(&old, &o, "title").as_str())?;
                tx.put(&obj, "created", get_i64(&old, &o, "created"))?;
                tx.put(&obj, "modified", get_i64(&old, &o, "modified"))?;
                if get_bool(&old, &o, "private") {
                    tx.put(&obj, "private", true)?;
                }
                let bl = tx.put_object(&obj, "body", ObjType::List)?;
                insert_lines(&mut tx, &bl, 0, &body_lines(&body))?;
                let fl = tx.put_object(&obj, "folder", ObjType::List)?;
                insert_lines(&mut tx, &fl, 0, &get_folder(&old, &o))?;
            }
        }
        if let Some(ot) = child(&old, &ROOT, "todos") {
            for id in old.keys(&ot) {
                let Some(o) = child(&old, &ot, &id) else { continue };
                let obj = tx.put_object(&todos, id.as_str(), ObjType::Map)?;
                tx.put(&obj, "text", get_str(&old, &o, "text").as_str())?;
                tx.put(&obj, "order", get_f64(&old, &o, "order"))?;
                tx.put(&obj, "created", get_i64(&old, &o, "created"))?;
                let tags = str_list(&old, &o, "tags", At::Now);
                if !tags.is_empty() {
                    let tl = tx.put_object(&obj, "tags", ObjType::List)?;
                    insert_lines(&mut tx, &tl, 0, &tags)?;
                }
            }
        }
        // Calendars (`cal/<id>` maps) and marks (`mark/<char>` scalars) live
        // straight on ROOT; copy them verbatim so subscriptions/bookmarks
        // survive the re-genesis.
        for k in old.keys(ROOT) {
            if k.starts_with(CAL_PREFIX) {
                if let Some(o) = child(&old, &ROOT, &k) {
                    let obj = tx.put_object(ROOT, k.as_str(), ObjType::Map)?;
                    tx.put(&obj, "url", get_str(&old, &o, "url").as_str())?;
                    tx.put(&obj, "name", get_str(&old, &o, "name").as_str())?;
                    tx.put(&obj, "created", get_i64(&old, &o, "created"))?;
                }
            } else if k.starts_with(MARK_PREFIX) {
                let v = get_str(&old, &ROOT, &k);
                if !v.is_empty() {
                    tx.put(ROOT, k.as_str(), v.as_str())?;
                }
            }
        }
        commit(tx);

        // Write the new doc, then move the original aside (first backup wins,
        // so re-running can't clobber the pristine v1 copy).
        let new_bytes = doc.save();
        let tmp = path.with_extension("automerge.tmp");
        std::fs::write(&tmp, &new_bytes).with_context(|| format!("writing {}", tmp.display()))?;
        let bak = path.with_extension("automerge.v1.bak");
        if !bak.exists() {
            std::fs::rename(path, &bak).context("backing up v1 replica")?;
        }
        std::fs::rename(&tmp, path).context("installing migrated replica")?;
        Ok(true)
    }

    // ---- Sync ----------------------------------------------------------
    //
    // Thin facade over `automerge`'s sync protocol so Automerge stays
    // entirely inside this module. The server/client transport layers deal
    // only in opaque `SyncState` + raw message bytes; they never see an
    // `Automerge`. Remote changes land via the low-level sync API, never
    // through `AutoCommit`.

    /// Produce the next sync message for a peer, or `None` if that peer is
    /// already up to date. `state` must be the same value across calls for
    /// the life of one peer connection.
    pub fn generate_sync_message(&mut self, state: &mut SyncState) -> Option<Vec<u8>> {
        self.doc
            .generate_sync_message(state)
            .map(AmSyncMessage::encode)
    }

    /// Apply a sync message received from a peer. Returns `true` if it
    /// changed the document (i.e. callers should persist / refresh).
    pub fn receive_sync_message(
        &mut self,
        state: &mut SyncState,
        msg: &[u8],
    ) -> Result<bool> {
        let msg = AmSyncMessage::decode(msg).context("decoding sync message")?;
        let heads_before = self.doc.get_heads();
        self.doc
            .receive_sync_message(state, msg)
            .context("applying sync message")?;
        // Re-resolve the cached container ids: a merge can change which
        // object wins at ROOT["notes"]/["todos"] (legacy docs predating the
        // shared genesis), and a stale id would silently read the wrong map.
        if let Ok(Some((Value::Object(_), id))) = self.doc.get(ROOT, "notes") {
            self.notes = id;
        }
        if let Ok(Some((Value::Object(_), id))) = self.doc.get(ROOT, "todos") {
            self.todos = id;
        }
        // Same defensive re-resolve for the lazily-created checkpoints map:
        // once a peer's first-checkpoint change arrives, pick it up so a
        // later list/restore sees it.
        if let Ok(Some((Value::Object(_), id))) = self.doc.get(ROOT, "checkpoints") {
            self.checkpoints = Some(id);
        }
        Ok(self.doc.get_heads() != heads_before)
    }

    // ---- Notes ---------------------------------------------------------

    pub fn notes(&self) -> Vec<Note> {
        self.doc
            .keys(&self.notes)
            .filter_map(|id| self.read_note(&id))
            .collect()
    }

    pub fn note(&self, id: &str) -> Option<Note> {
        self.read_note(id)
    }

    /// Body-free listing — does NOT materialize note `Text`. Use this for
    /// the folder tree; reserve [`Store::notes`]/[`Store::note`] for when
    /// the body is actually needed.
    pub fn note_metas(&self) -> Vec<NoteMeta> {
        self.doc
            .keys(&self.notes)
            .filter_map(|id| {
                let obj = child(&self.doc, &self.notes, &id)?;
                Some(NoteMeta {
                    id,
                    title: get_str(&self.doc, &obj, "title"),
                    folder: get_folder(&self.doc, &obj),
                    created: get_i64(&self.doc, &obj, "created"),
                    modified: get_i64(&self.doc, &obj, "modified"),
                    private: get_bool(&self.doc, &obj, "private"),
                })
            })
            .collect()
    }

    /// Case-insensitive substring search over title + folder path, plus
    /// body — except a **private** note's body is only searched when
    /// `include_private_bodies` is true. The MCP layer passes `false`, so
    /// the assistant can find/organise a private note by its (already
    /// visible) title but can never probe its hidden contents by querying
    /// for a phrase and seeing it match. Body materialization is the
    /// expensive `Text` path, done only when title/folder miss — fine for
    /// an explicit search, like opening a note. Empty query → no results.
    pub fn search_notes(&self, query: &str, include_private_bodies: bool) -> Vec<NoteMeta> {
        self.search_notes_snippets(query, include_private_bodies)
            .into_iter()
            .map(|(m, _)| m)
            .collect()
    }

    /// As [`Store::search_notes`], but each hit is paired with an optional
    /// snippet: a one-line window of body text around the first match, so a
    /// caller can triage results without a follow-up `get_note` per hit. The
    /// snippet is `None` when the match was on title/folder only (the title
    /// is already returned) or when the body wasn't searched — in
    /// particular a **private** note never yields a body snippet, preserving
    /// the same boundary `search_notes` enforces.
    pub fn search_notes_snippets(
        &self,
        query: &str,
        include_private_bodies: bool,
    ) -> Vec<(NoteMeta, Option<String>)> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        self.doc
            .keys(&self.notes)
            .filter_map(|id| {
                let obj = child(&self.doc, &self.notes, &id)?;
                let title = get_str(&self.doc, &obj, "title");
                let folder = get_folder(&self.doc, &obj);
                let private = get_bool(&self.doc, &obj, "private");
                let meta_hit = title.to_lowercase().contains(&q)
                    || folder.join("/").to_lowercase().contains(&q);
                // Materialize the body only when title/folder miss and the
                // body is searchable (never for a private note at the MCP
                // boundary) — same cost profile as before.
                let body = (!meta_hit && (!private || include_private_bodies))
                    .then(|| {
                        child(&self.doc, &obj, "body")
                            .map(|b| list_strs(&self.doc, &b, At::Now).join("\n"))
                    })
                    .flatten();
                let body_hit = body
                    .as_deref()
                    .map(|t| t.to_lowercase().contains(&q))
                    .unwrap_or(false);
                if !meta_hit && !body_hit {
                    return None;
                }
                let snippet = body_hit
                    .then(|| body.as_deref().and_then(|t| make_snippet(t, &q)))
                    .flatten();
                Some((
                    NoteMeta {
                        id,
                        title,
                        folder,
                        created: get_i64(&self.doc, &obj, "created"),
                        modified: get_i64(&self.doc, &obj, "modified"),
                        private,
                    },
                    snippet,
                ))
            })
            .collect()
    }

    /// The derived folder tree with note counts — answers "what folders do
    /// I have?" in one call, without shipping every note record. For each
    /// distinct folder path that appears (every note's path *and every
    /// ancestor prefix* of it, so parent folders with no direct notes still
    /// show up), `direct` is notes filed exactly there and `subtree` is
    /// notes there or anywhere below. Root notes (empty path) are reported
    /// as path `[]`. Sorted lexicographically by path. Body-free.
    pub fn folder_tree(&self) -> Vec<(Vec<String>, usize, usize)> {
        use std::collections::BTreeMap;
        let mut direct: BTreeMap<Vec<String>, usize> = BTreeMap::new();
        let mut subtree: BTreeMap<Vec<String>, usize> = BTreeMap::new();
        for m in self.note_metas() {
            *direct.entry(m.folder.clone()).or_default() += 1;
            // Count this note against every ancestor prefix (1..=len);
            // the empty-root prefix is tracked via `direct[[]]` only.
            for k in 1..=m.folder.len() {
                *subtree.entry(m.folder[..k].to_vec()).or_default() += 1;
            }
        }
        let mut paths: std::collections::BTreeSet<Vec<String>> =
            subtree.keys().cloned().collect();
        paths.extend(direct.keys().cloned());
        paths
            .into_iter()
            .map(|p| {
                let d = direct.get(&p).copied().unwrap_or(0);
                // A leaf folder has no subtree entry of its own; its
                // subtree total is just its direct count.
                let s = subtree.get(&p).copied().unwrap_or(d).max(d);
                (p, d, s)
            })
            .collect()
    }

    fn read_note(&self, id: &str) -> Option<Note> {
        note_view(&self.doc, &self.notes, id, At::Now)
    }

    /// Create an empty note in `folder`, returning its new id.
    pub fn create_note(&mut self, folder: &[String], title: &str) -> Result<String> {
        let id = new_id();
        let ts = now_ms();
        let mut tx = self.doc.transaction();
        let obj = tx.put_object(&self.notes, id.as_str(), ObjType::Map)?;
        tx.put(&obj, "title", title)?;
        tx.put(&obj, "created", ts)?;
        tx.put(&obj, "modified", ts)?;
        // Empty body = empty line list; the first edit splices lines in.
        tx.put_object(&obj, "body", ObjType::List)?;
        let list = tx.put_object(&obj, "folder", ObjType::List)?;
        for (i, c) in folder.iter().enumerate() {
            tx.insert(&list, i, c.as_str())?;
        }
        commit(tx);
        Ok(id)
    }

    /// Bulk-import notes in a single transaction (the only fast path at
    /// scale). The body goes in as one list element per line.
    pub fn import_notes(
        &mut self,
        items: &[NoteInput],
        mut progress: impl FnMut(usize),
    ) -> Result<usize> {
        let mut tx = self.doc.transaction();
        for (idx, n) in items.iter().enumerate() {
            let id = new_id();
            let obj = tx.put_object(&self.notes, id.as_str(), ObjType::Map)?;
            tx.put(&obj, "title", n.title.as_str())?;
            tx.put(&obj, "created", n.created)?;
            tx.put(&obj, "modified", n.modified)?;
            let body = tx.put_object(&obj, "body", ObjType::List)?;
            insert_lines(&mut tx, &body, 0, &body_lines(&n.body))?;
            let list = tx.put_object(&obj, "folder", ObjType::List)?;
            for (i, c) in n.folder.iter().enumerate() {
                tx.insert(&list, i, c.as_str())?;
            }
            progress(idx + 1);
        }
        commit(tx);
        Ok(items.len())
    }

    pub fn set_note_title(&mut self, id: &str, title: &str) -> Result<()> {
        let obj = child(&self.doc, &self.notes, id).ok_or_else(|| anyhow!("no such note"))?;
        let mut tx = self.doc.transaction();
        tx.put(&obj, "title", title)?;
        tx.put(&obj, "modified", now_ms())?;
        commit(tx);
        Ok(())
    }

    /// Mark a note private (or not). Privacy is enforced only at the MCP
    /// boundary; this is a plain field write that rides sync like
    /// title/folder. **Not retroactive:** it protects the note from now on,
    /// not snapshots/history that predate it.
    pub fn set_note_private(&mut self, id: &str, private: bool) -> Result<()> {
        let obj = child(&self.doc, &self.notes, id).ok_or_else(|| anyhow!("no such note"))?;
        let mut tx = self.doc.transaction();
        tx.put(&obj, "private", private)?;
        tx.put(&obj, "modified", now_ms())?;
        commit(tx);
        Ok(())
    }

    pub fn set_note_folder(&mut self, id: &str, folder: &[String]) -> Result<()> {
        let obj = child(&self.doc, &self.notes, id).ok_or_else(|| anyhow!("no such note"))?;
        let mut tx = self.doc.transaction();
        let list = tx.put_object(&obj, "folder", ObjType::List)?;
        for (i, c) in folder.iter().enumerate() {
            tx.insert(&list, i, c.as_str())?;
        }
        tx.put(&obj, "modified", now_ms())?;
        commit(tx);
        Ok(())
    }

    /// Update the note body, splicing only the changed *lines* so a
    /// concurrent edit to a different line still merges.
    pub fn set_note_body(&mut self, id: &str, new: &str) -> Result<()> {
        let obj = child(&self.doc, &self.notes, id).ok_or_else(|| anyhow!("no such note"))?;
        let body = child(&self.doc, &obj, "body")
            .ok_or_else(|| anyhow!("note has no body object"))?;

        let cur = list_strs(&self.doc, &body, At::Now);
        let Some((p, del, ins)) = lines_splice(&cur, &body_lines(new)) else {
            return Ok(());
        };
        let mut tx = self.doc.transaction();
        delete_lines(&mut tx, &body, p, del)?;
        insert_lines(&mut tx, &body, p, &ins)?;
        tx.put(&obj, "modified", now_ms())?;
        commit(tx);
        Ok(())
    }

    /// Recursively rename a folder: every note whose path begins with
    /// `path` has the component at `path.len()-1` replaced with `new_name`,
    /// so the whole subtree moves with it. Returns the number of notes
    /// updated. One transaction, and bodies are untouched, so a concurrent
    /// body edit on another replica still merges at character granularity.
    pub fn rename_folder(&mut self, path: &[String], new_name: &str) -> Result<usize> {
        if path.is_empty() {
            return Err(anyhow!("cannot rename the root"));
        }
        let depth = path.len() - 1;
        // Resolve everything under the read borrow first, then mutate.
        let updates: Vec<(String, Vec<String>)> = self
            .doc
            .keys(&self.notes)
            .filter_map(|id| {
                let obj = child(&self.doc, &self.notes, &id)?;
                let folder = get_folder(&self.doc, &obj);
                if folder.len() > depth && folder[..path.len()] == *path {
                    let mut nf = folder;
                    nf[depth] = new_name.to_string();
                    Some((id, nf))
                } else {
                    None
                }
            })
            .collect();
        if updates.is_empty() {
            return Ok(0);
        }
        let ts = now_ms();
        let mut tx = self.doc.transaction();
        for (id, nf) in &updates {
            let Some((Value::Object(_), obj)) = tx.get(&self.notes, id.as_str())? else {
                continue;
            };
            let list = tx.put_object(&obj, "folder", ObjType::List)?;
            for (i, c) in nf.iter().enumerate() {
                tx.insert(&list, i, c.as_str())?;
            }
            tx.put(&obj, "modified", ts)?;
        }
        commit(tx);
        Ok(updates.len())
    }

    /// Move many notes to one destination folder in a **single
    /// transaction** (one change, one sync broadcast, one undo unit —
    /// unlike N separate `set_note_folder` calls). Unknown ids are skipped;
    /// returns how many notes actually moved.
    pub fn move_notes(&mut self, ids: &[String], folder: &[String]) -> Result<usize> {
        let ts = now_ms();
        let mut tx = self.doc.transaction();
        let mut moved = 0;
        for id in ids {
            let Some((Value::Object(_), obj)) = tx.get(&self.notes, id.as_str())? else {
                continue;
            };
            let list = tx.put_object(&obj, "folder", ObjType::List)?;
            for (i, c) in folder.iter().enumerate() {
                tx.insert(&list, i, c.as_str())?;
            }
            tx.put(&obj, "modified", ts)?;
            moved += 1;
        }
        commit(tx);
        Ok(moved)
    }

    /// Move/rename a whole folder *subtree*: every note whose path begins
    /// with `from` has that prefix replaced by `to`. Unlike
    /// [`Store::rename_folder`] (which only renames the final component
    /// in place) this can change depth and re-parent — e.g.
    /// `["gometro"]` → `["consulting","oldclients","gometro"]`. One
    /// transaction; bodies untouched so concurrent edits still merge.
    /// Returns the number of notes updated. `from` must be non-empty
    /// (the root is not a movable folder).
    pub fn move_folder(&mut self, from: &[String], to: &[String]) -> Result<usize> {
        if from.is_empty() {
            return Err(anyhow!("cannot move the root folder"));
        }
        let updates: Vec<(String, Vec<String>)> = self
            .doc
            .keys(&self.notes)
            .filter_map(|id| {
                let obj = child(&self.doc, &self.notes, &id)?;
                let folder = get_folder(&self.doc, &obj);
                if folder.len() >= from.len() && folder[..from.len()] == *from {
                    let mut nf = to.to_vec();
                    nf.extend_from_slice(&folder[from.len()..]);
                    Some((id, nf))
                } else {
                    None
                }
            })
            .collect();
        if updates.is_empty() {
            return Ok(0);
        }
        let ts = now_ms();
        let mut tx = self.doc.transaction();
        for (id, nf) in &updates {
            let Some((Value::Object(_), obj)) = tx.get(&self.notes, id.as_str())? else {
                continue;
            };
            let list = tx.put_object(&obj, "folder", ObjType::List)?;
            for (i, c) in nf.iter().enumerate() {
                tx.insert(&list, i, c.as_str())?;
            }
            tx.put(&obj, "modified", ts)?;
        }
        commit(tx);
        Ok(updates.len())
    }

    pub fn delete_note(&mut self, id: &str) -> Result<()> {
        let mut tx = self.doc.transaction();
        tx.delete(&self.notes, id)?;
        commit(tx);
        Ok(())
    }

    // ---- Todos ---------------------------------------------------------

    pub fn todos(&self) -> Vec<Todo> {
        let mut v: Vec<Todo> = self
            .doc
            .keys(&self.todos)
            .filter_map(|id| {
                let obj = child(&self.doc, &self.todos, &id)?;
                Some(Todo {
                    id: id.clone(),
                    text: get_str(&self.doc, &obj, "text"),
                    order: get_f64(&self.doc, &obj, "order"),
                    created: get_i64(&self.doc, &obj, "created"),
                    tags: str_list(&self.doc, &obj, "tags", At::Now),
                })
            })
            .collect();
        v.sort_by(|a, b| {
            a.order
                .partial_cmp(&b.order)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        v
    }

    pub fn add_todo(&mut self, text: &str) -> Result<String> {
        let next = self.todos().last().map(|t| t.order + 1.0).unwrap_or(1.0);
        let id = new_id();
        let mut tx = self.doc.transaction();
        let obj = tx.put_object(&self.todos, id.as_str(), ObjType::Map)?;
        tx.put(&obj, "text", text)?;
        tx.put(&obj, "order", next)?;
        tx.put(&obj, "created", now_ms())?;
        commit(tx);
        Ok(id)
    }

    pub fn set_todo_text(&mut self, id: &str, text: &str) -> Result<()> {
        let obj = child(&self.doc, &self.todos, id).ok_or_else(|| anyhow!("no such todo"))?;
        let mut tx = self.doc.transaction();
        tx.put(&obj, "text", text)?;
        commit(tx);
        Ok(())
    }

    /// Swap the sort keys of two todos (one-step reordering).
    pub fn swap_todo_order(&mut self, a: &str, b: &str) -> Result<()> {
        let oa = child(&self.doc, &self.todos, a).ok_or_else(|| anyhow!("no such todo"))?;
        let ob = child(&self.doc, &self.todos, b).ok_or_else(|| anyhow!("no such todo"))?;
        let va = get_f64(&self.doc, &oa, "order");
        let vb = get_f64(&self.doc, &ob, "order");
        let mut tx = self.doc.transaction();
        tx.put(&oa, "order", vb)?;
        tx.put(&ob, "order", va)?;
        commit(tx);
        Ok(())
    }

    pub fn delete_todo(&mut self, id: &str) -> Result<()> {
        let mut tx = self.doc.transaction();
        tx.delete(&self.todos, id)?;
        commit(tx);
        Ok(())
    }

    /// Add `tag` to every given todo (idempotent per todo) in a **single
    /// transaction** — one change, one sync broadcast, like `move_notes`.
    /// Unknown ids and todos that already carry the tag are skipped;
    /// returns how many todos actually gained it.
    pub fn tag_todos(&mut self, ids: &[String], tag: &str) -> Result<usize> {
        let tag = tag.trim();
        if tag.is_empty() {
            return Ok(0);
        }
        // Resolve each todo's new tag list under the read borrow, then
        // mutate (same shape as `rename_folder`).
        let updates: Vec<(String, Vec<String>)> = ids
            .iter()
            .filter_map(|id| {
                let obj = child(&self.doc, &self.todos, id)?;
                let mut tags = str_list(&self.doc, &obj, "tags", At::Now);
                if tags.iter().any(|t| t == tag) {
                    return None;
                }
                tags.push(tag.to_string());
                Some((id.clone(), tags))
            })
            .collect();
        if updates.is_empty() {
            return Ok(0);
        }
        let mut tx = self.doc.transaction();
        for (id, tags) in &updates {
            let Some((Value::Object(_), obj)) = tx.get(&self.todos, id.as_str())? else {
                continue;
            };
            let list = tx.put_object(&obj, "tags", ObjType::List)?;
            for (i, c) in tags.iter().enumerate() {
                tx.insert(&list, i, c.as_str())?;
            }
        }
        commit(tx);
        Ok(updates.len())
    }

    /// Replace a todo's entire tag set (used by restore reconciliation).
    pub fn set_todo_tags(&mut self, id: &str, tags: &[String]) -> Result<()> {
        let obj = child(&self.doc, &self.todos, id).ok_or_else(|| anyhow!("no such todo"))?;
        let mut tx = self.doc.transaction();
        let list = tx.put_object(&obj, "tags", ObjType::List)?;
        for (i, c) in tags.iter().enumerate() {
            tx.insert(&list, i, c.as_str())?;
        }
        commit(tx);
        Ok(())
    }

    // ---- Calendars -----------------------------------------------------
    //
    // Only the *subscription* lives in the doc (and thus syncs to the
    // server and other devices). Each is its own `ROOT["cal/<uuid>"]` map:
    // the uuid is unique and created by exactly one replica, so — unlike a
    // shared `calendars` container — there is no concurrent-seed divergence
    // and no need to touch `genesis_doc`. The fetched events are a derived,
    // non-synced local cache (see the TUI's `cal_sync`).

    /// Subscribe to an ICS calendar URL. `name` may be empty; the fetch
    /// thread backfills it from the feed's `X-WR-CALNAME`.
    pub fn add_calendar(&mut self, url: &str, name: &str) -> Result<String> {
        let id = new_id();
        let key = format!("{CAL_PREFIX}{id}");
        let ts = now_ms();
        let mut tx = self.doc.transaction();
        let obj = tx.put_object(ROOT, key.as_str(), ObjType::Map)?;
        tx.put(&obj, "url", url)?;
        tx.put(&obj, "name", name)?;
        tx.put(&obj, "created", ts)?;
        commit(tx);
        Ok(id)
    }

    /// Set the display name (used to apply `X-WR-CALNAME` after a fetch, or
    /// a user rename). No-op-safe if the calendar is gone.
    pub fn set_calendar_name(&mut self, id: &str, name: &str) -> Result<()> {
        let key = format!("{CAL_PREFIX}{id}");
        let Some(obj) = child(&self.doc, &ROOT, &key) else {
            return Ok(());
        };
        let mut tx = self.doc.transaction();
        tx.put(&obj, "name", name)?;
        commit(tx);
        Ok(())
    }

    pub fn remove_calendar(&mut self, id: &str) -> Result<()> {
        let key = format!("{CAL_PREFIX}{id}");
        let mut tx = self.doc.transaction();
        tx.delete(ROOT, key.as_str())?;
        commit(tx);
        Ok(())
    }

    /// All subscriptions, by name (case-insensitive), ties by id.
    pub fn calendars(&self) -> Vec<Calendar> {
        let mut v: Vec<Calendar> = self
            .doc
            .keys(ROOT)
            .filter(|k| k.starts_with(CAL_PREFIX))
            .filter_map(|k| {
                let obj = child(&self.doc, &ROOT, &k)?;
                Some(Calendar {
                    id: k[CAL_PREFIX.len()..].to_string(),
                    url: get_str(&self.doc, &obj, "url"),
                    name: get_str(&self.doc, &obj, "name"),
                    created: get_i64(&self.doc, &obj, "created"),
                })
            })
            .collect();
        v.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        v
    }

    // ---- Marks ---------------------------------------------------------
    //
    // Global note bookmarks: a single user-chosen char keyed straight on
    // ROOT (`mark/<char>`) holding the target note id as a scalar string.
    // No container is ever created, so there is nothing for two clients to
    // concurrently seed — the divergence hazard simply does not exist here.

    /// Point bookmark `key` at `note_id` (overwrites any previous target).
    pub fn set_mark(&mut self, key: char, note_id: &str) -> Result<()> {
        let k = format!("{MARK_PREFIX}{key}");
        let mut tx = self.doc.transaction();
        tx.put(ROOT, k.as_str(), note_id)?;
        commit(tx);
        Ok(())
    }

    /// The note id bookmark `key` points at, if it has ever been set.
    pub fn mark(&self, key: char) -> Option<String> {
        let k = format!("{MARK_PREFIX}{key}");
        let s = get_str(&self.doc, &ROOT, &k);
        (!s.is_empty()).then_some(s)
    }

    /// Every set bookmark, as `(key, note_id)`, sorted by key.
    pub fn marks(&self) -> Vec<(char, String)> {
        let mut v: Vec<(char, String)> = self
            .doc
            .keys(ROOT)
            .filter(|k| k.starts_with(MARK_PREFIX))
            .filter_map(|k| {
                let key = k[MARK_PREFIX.len()..].chars().next()?;
                let id = get_str(&self.doc, &ROOT, &k);
                (!id.is_empty()).then_some((key, id))
            })
            .collect();
        v.sort_by_key(|(k, _)| *k);
        v
    }

    // ---- Checkpoints ---------------------------------------------------
    //
    // A checkpoint copies nothing: Automerge already keeps the entire op
    // history, so it is just a label + the document heads at creation. A
    // CRDT cannot rewind, so `restore` does not delete history — it writes
    // a FORWARD change that reconciles the live corpus back to the
    // checkpoint's state. That change then syncs and persists through the
    // exact same path as any edit (one transaction → heads advance →
    // server rebroadcast + debounced save), so the sync layer needs no
    // special case at all.

    /// Resolve (or, on first ever use, create) `ROOT["checkpoints"]`. See
    /// the field comment: lazy single-writer creation is what keeps this
    /// off the genesis hazard.
    fn ensure_checkpoints(&mut self) -> Result<ObjId> {
        if let Some(id) = &self.checkpoints {
            return Ok(id.clone());
        }
        if let Ok(Some((Value::Object(_), id))) = self.doc.get(ROOT, "checkpoints") {
            self.checkpoints = Some(id.clone());
            return Ok(id);
        }
        let mut tx = self.doc.transaction();
        let id = tx.put_object(ROOT, "checkpoints", ObjType::Map)?;
        commit(tx);
        self.checkpoints = Some(id.clone());
        Ok(id)
    }

    /// Record a restore point for the *current* state. `reason` is the
    /// LLM's (or user's) description of what the surrounding batch is.
    pub fn create_checkpoint(&mut self, reason: &str) -> Result<String> {
        // Heads of the state we are remembering — captured before the
        // bookkeeping write so the checkpoint names the data as it is now.
        let heads = self.doc.get_heads();
        let cp = self.ensure_checkpoints()?;
        let id = new_id();
        let ts = now_ms();
        let mut tx = self.doc.transaction();
        let obj = tx.put_object(&cp, id.as_str(), ObjType::Map)?;
        tx.put(&obj, "reason", reason)?;
        tx.put(&obj, "created", ts)?;
        let hl = tx.put_object(&obj, "heads", ObjType::List)?;
        for (i, h) in heads.iter().enumerate() {
            tx.insert(&hl, i, h.to_string().as_str())?;
        }
        commit(tx);
        Ok(id)
    }

    /// All checkpoints, newest first.
    pub fn checkpoints(&self) -> Vec<Checkpoint> {
        let Some(cp) = &self.checkpoints else {
            return Vec::new();
        };
        let mut v: Vec<Checkpoint> = self
            .doc
            .keys(cp)
            .filter_map(|id| {
                let obj = child(&self.doc, cp, &id)?;
                Some(Checkpoint {
                    id,
                    reason: get_str(&self.doc, &obj, "reason"),
                    created: get_i64(&self.doc, &obj, "created"),
                })
            })
            .collect();
        v.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
        v
    }

    fn checkpoint_heads(&self, id: &str) -> Result<Vec<ChangeHash>> {
        let cp = self
            .checkpoints
            .as_ref()
            .ok_or_else(|| anyhow!("no checkpoints exist yet"))?;
        let obj = child(&self.doc, cp, id).ok_or_else(|| anyhow!("no such checkpoint"))?;
        let hl = child(&self.doc, &obj, "heads")
            .ok_or_else(|| anyhow!("checkpoint has no heads"))?;
        let mut hs = Vec::new();
        for i in 0..self.doc.length(&hl) {
            if let Ok(Some((Value::Scalar(s), _))) = self.doc.get(&hl, i) {
                if let ScalarValue::Str(st) = s.as_ref() {
                    hs.push(
                        ChangeHash::from_str(st)
                            .map_err(|e| anyhow!("corrupt checkpoint head: {e}"))?,
                    );
                }
            }
        }
        Ok(hs)
    }

    fn plan_restore(&self, id: &str) -> Result<RestorePlan> {
        let heads = self.checkpoint_heads(id)?;
        self.plan_restore_to(&heads)
    }

    /// Diff the document state *at `heads`* against the live corpus. Shared
    /// by checkpoint restore and arbitrary-point ("time-travel") restore;
    /// `preview_*` use the report, `restore_*` apply the plan.
    fn plan_restore_to(&self, heads: &[ChangeHash]) -> Result<RestorePlan> {
        // Read the past state with Automerge's clock-based `*_at` API
        // instead of `fork_at`. `fork_at`/`get_changes` reconstruct changes
        // from the op-set and `unwrap()` an internal `MissingOps` error
        // when the op range has holes (normal after sync/merge) — an
        // upstream 0.7.x panic. The `*_at` reads walk the live op-set under
        // a vector clock derived from `heads`: no reconstruction, no panic,
        // and no document clone. `notes`/`todos` are genesis objects so
        // their ids are valid at any point in history.
        //
        // Guard first: unknown heads would make every `*_at` read empty and
        // a restore would then *delete the entire corpus*. Refuse instead.
        for h in heads {
            if self.doc.get_change_meta_by_hash(h).is_none() {
                return Err(anyhow!(
                    "restore target {h} is not in local history yet (sync first)"
                ));
            }
        }
        let target_notes = all_notes(&self.doc, &self.notes, At::Heads(heads));
        let target_todos = all_todos(&self.doc, &self.todos, At::Heads(heads));
        let live_notes = all_notes(&self.doc, &self.notes, At::Now);
        let live_todos = all_todos(&self.doc, &self.todos, At::Now);

        use std::collections::{BTreeMap, BTreeSet};
        let live_n: BTreeMap<&str, &Note> =
            live_notes.iter().map(|n| (n.id.as_str(), n)).collect();
        let tgt_n_ids: BTreeSet<&str> =
            target_notes.iter().map(|n| n.id.as_str()).collect();
        let live_t: BTreeMap<&str, &Todo> =
            live_todos.iter().map(|t| (t.id.as_str(), t)).collect();
        let tgt_t_ids: BTreeSet<&str> =
            target_todos.iter().map(|t| t.id.as_str()).collect();

        let mut rep = RestoreReport::default();
        let mut notes_set = Vec::new();
        for t in &target_notes {
            match live_n.get(t.id.as_str()) {
                None => {
                    rep.notes_added += 1;
                    notes_set.push(t.clone());
                }
                Some(l) => {
                    if l.title != t.title
                        || l.folder != t.folder
                        || l.body != t.body
                        || l.created != t.created
                        || l.private != t.private
                    {
                        rep.notes_updated += 1;
                        notes_set.push(t.clone());
                    }
                }
            }
        }
        let notes_del: Vec<String> = live_notes
            .iter()
            .filter(|l| !tgt_n_ids.contains(l.id.as_str()))
            .map(|l| l.id.clone())
            .collect();
        rep.notes_deleted = notes_del.len();

        let mut todos_set = Vec::new();
        for t in &target_todos {
            match live_t.get(t.id.as_str()) {
                None => {
                    rep.todos_added += 1;
                    todos_set.push(t.clone());
                }
                Some(l) => {
                    if l.text != t.text
                        || l.order != t.order
                        || l.created != t.created
                        || l.tags != t.tags
                    {
                        rep.todos_updated += 1;
                        todos_set.push(t.clone());
                    }
                }
            }
        }
        let todos_del: Vec<String> = live_todos
            .iter()
            .filter(|l| !tgt_t_ids.contains(l.id.as_str()))
            .map(|l| l.id.clone())
            .collect();
        rep.todos_deleted = todos_del.len();

        Ok(RestorePlan {
            notes_set,
            notes_del,
            todos_set,
            todos_del,
            report: rep,
        })
    }

    /// What `restore_checkpoint(id)` would change, without changing it.
    pub fn preview_restore(&self, id: &str) -> Result<RestoreReport> {
        Ok(self.plan_restore(id)?.report)
    }

    /// What restoring to an arbitrary change `hash` (time-travel) would
    /// change, without changing it.
    pub fn preview_restore_to(&self, hash: &str) -> Result<RestoreReport> {
        let h = ChangeHash::from_str(hash).map_err(|e| anyhow!("bad change hash: {e}"))?;
        Ok(self.plan_restore_to(&[h])?.report)
    }

    /// Reconcile the whole corpus back to a checkpoint in one transaction.
    /// Whole-corpus by design: this also reverts edits made *after* the
    /// checkpoint (incl. on other devices) — acceptable under the
    /// single-operator model, and the returned report shows the extent.
    pub fn restore_checkpoint(&mut self, id: &str) -> Result<RestoreReport> {
        let plan = self.plan_restore(id)?;
        self.apply_restore(plan)
    }

    /// Time-travel restore: reconcile the whole corpus to its state at an
    /// arbitrary change `hash` from the History timeline. Same engine as
    /// checkpoint restore — it's just another forward change, so it syncs
    /// and persists normally.
    pub fn restore_to(&mut self, hash: &str) -> Result<RestoreReport> {
        let h = ChangeHash::from_str(hash).map_err(|e| anyhow!("bad change hash: {e}"))?;
        let plan = self.plan_restore_to(&[h])?;
        self.apply_restore(plan)
    }

    fn apply_restore(&mut self, plan: RestorePlan) -> Result<RestoreReport> {
        let ts = now_ms();
        let mut tx = self.doc.transaction();

        for did in &plan.notes_del {
            tx.delete(&self.notes, did.as_str())?;
        }
        for n in &plan.notes_set {
            let obj = match tx.get(&self.notes, n.id.as_str())? {
                Some((Value::Object(_), o)) => o,
                _ => tx.put_object(&self.notes, n.id.as_str(), ObjType::Map)?,
            };
            tx.put(&obj, "title", n.title.as_str())?;
            tx.put(&obj, "created", n.created)?;
            tx.put(&obj, "modified", ts)?;
            // `private` is part of the note's state at the checkpoint, so
            // it reconciles like any field. Privacy is non-retroactive by
            // design: restoring to a pre-private snapshot legitimately
            // returns that older (then-unprotected) state.
            tx.put(&obj, "private", n.private)?;
            let fl = tx.put_object(&obj, "folder", ObjType::List)?;
            for (i, c) in n.folder.iter().enumerate() {
                tx.insert(&fl, i, c.as_str())?;
            }
            // Splice only the changed lines so a concurrent edit to another
            // line still merges, exactly like `set_note_body`.
            let body = match tx.get(&obj, "body")? {
                Some((Value::Object(ObjType::List), b)) => b,
                // Missing or a legacy `Text` body (pre-migration history a
                // restore can reach) → (re)create as the line list.
                _ => tx.put_object(&obj, "body", ObjType::List)?,
            };
            let cur = list_strs(&tx, &body, At::Now);
            if let Some((p, del, ins)) = lines_splice(&cur, &body_lines(&n.body)) {
                delete_lines(&mut tx, &body, p, del)?;
                insert_lines(&mut tx, &body, p, &ins)?;
            }
        }

        for did in &plan.todos_del {
            tx.delete(&self.todos, did.as_str())?;
        }
        for t in &plan.todos_set {
            let obj = match tx.get(&self.todos, t.id.as_str())? {
                Some((Value::Object(_), o)) => o,
                _ => tx.put_object(&self.todos, t.id.as_str(), ObjType::Map)?,
            };
            tx.put(&obj, "text", t.text.as_str())?;
            tx.put(&obj, "order", t.order)?;
            tx.put(&obj, "created", t.created)?;
            let tl = tx.put_object(&obj, "tags", ObjType::List)?;
            for (i, c) in t.tags.iter().enumerate() {
                tx.insert(&tl, i, c.as_str())?;
            }
        }

        commit(tx);
        Ok(plan.report)
    }

    // ---- Edit history --------------------------------------------------

    /// The full edit timeline, newest first: every Automerge change as a
    /// pure [`HistoryRow`]. Rows whose hash is a checkpoint head carry that
    /// checkpoint's `reason`, so the History view shows named snapshots
    /// inline with the raw edits. Any row's `hash` can be fed to
    /// [`Store::restore_to`] / [`Store::preview_restore_to`].
    pub fn history(&self) -> Vec<HistoryRow> {
        // hash hex -> checkpoint reason, for the rows that are snapshots.
        let mut marks: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Some(cp) = &self.checkpoints {
            for id in self.doc.keys(cp) {
                let Some(obj) = child(&self.doc, cp, &id) else {
                    continue;
                };
                let reason = get_str(&self.doc, &obj, "reason");
                if let Some(hl) = child(&self.doc, &obj, "heads") {
                    for i in 0..self.doc.length(&hl) {
                        if let Ok(Some((Value::Scalar(s), _))) = self.doc.get(&hl, i) {
                            if let ScalarValue::Str(h) = s.as_ref() {
                                marks.insert(h.to_string(), reason.clone());
                            }
                        }
                    }
                }
            }
        }
        // `get_changes_meta` reads ONLY the change graph — it does not
        // reconstruct ops, so it sidesteps the upstream 0.7.x `MissingOps`
        // panic that `get_changes` hits once history has merge holes.
        let mut rows: Vec<HistoryRow> = self
            .doc
            .get_changes_meta(&[])
            .into_iter()
            .map(|c| {
                let hash = c.hash.to_string();
                let ops = if c.max_op >= c.start_op {
                    (c.max_op - c.start_op + 1) as usize
                } else {
                    0
                };
                HistoryRow {
                    ts: if c.timestamp > 0 { c.timestamp * 1000 } else { 0 },
                    ops,
                    actor: c.actor.to_string().chars().take(8).collect(),
                    checkpoint: marks.get(&hash).cloned(),
                    hash,
                }
            })
            .collect();
        rows.reverse(); // newest first for display
        rows
    }
}

/// Private restore plan: the minimal upserts/deletes to make the corpus
/// equal the checkpoint. `*_set` are full target entities (recreate if
/// missing, overwrite the changed fields if present).
struct RestorePlan {
    notes_set: Vec<Note>,
    notes_del: Vec<String>,
    todos_set: Vec<Todo>,
    todos_del: Vec<String>,
    report: RestoreReport,
}

// ---- Free read helpers (work against any ReadDoc) ----------------------

// One reader family, optionally "as at heads". `At::Now` reads the live
// doc; `At::Heads(h)` reads the clock-based `*_at` view (used by restore —
// never `fork_at`, which hits the upstream `MissingOps` panic). Threading
// `At` through a single set of readers means note/todo materialization has
// exactly ONE definition, so a new field can't be added to one path and
// silently forgotten on the other.
#[derive(Clone, Copy)]
enum At<'a> {
    Now,
    Heads(&'a [ChangeHash]),
}

fn aget<'a, D: ReadDoc>(
    d: &'a D,
    obj: &ObjId,
    prop: impl Into<Prop>,
    at: At,
) -> Option<(Value<'a>, ObjId)> {
    match at {
        At::Now => d.get(obj, prop),
        At::Heads(h) => d.get_at(obj, prop, h),
    }
    .ok()
    .flatten()
}

fn akeys<D: ReadDoc>(d: &D, obj: &ObjId, at: At) -> Vec<String> {
    match at {
        At::Now => d.keys(obj).collect(),
        At::Heads(h) => d.keys_at(obj, h).collect(),
    }
}

fn alen<D: ReadDoc>(d: &D, list: &ObjId, at: At) -> usize {
    match at {
        At::Now => d.length(list),
        At::Heads(h) => d.length_at(list, h),
    }
}

fn as_obj(v: Option<(Value<'_>, ObjId)>) -> Option<ObjId> {
    match v {
        Some((Value::Object(_), id)) => Some(id),
        _ => None,
    }
}

fn as_str(v: Option<(Value<'_>, ObjId)>) -> String {
    match v {
        Some((Value::Scalar(s), _)) => match s.as_ref() {
            ScalarValue::Str(s) => s.to_string(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn as_i64(v: Option<(Value<'_>, ObjId)>) -> i64 {
    match v {
        Some((Value::Scalar(s), _)) => match s.as_ref() {
            ScalarValue::Int(i) => *i,
            ScalarValue::Uint(u) => *u as i64,
            ScalarValue::Timestamp(t) => *t,
            _ => 0,
        },
        _ => 0,
    }
}

fn as_f64(v: Option<(Value<'_>, ObjId)>) -> f64 {
    match v {
        Some((Value::Scalar(s), _)) => match s.as_ref() {
            ScalarValue::F64(f) => *f,
            ScalarValue::Int(i) => *i as f64,
            _ => 0.0,
        },
        _ => 0.0,
    }
}

/// A one-line search snippet: the first match of `q` (already lowercased)
/// in `body`, with up to `PAD` chars of context either side, whitespace
/// collapsed, ellipsed when clipped. `None` if `q` isn't actually in
/// `body` (callers only ask after a confirmed substring hit). Byte offsets
/// are snapped to char boundaries so multi-byte UTF-8 never panics.
fn make_snippet(body: &str, q: &str) -> Option<String> {
    const PAD: usize = 70;
    let at = body.to_lowercase().find(q)?;
    let lo = body[..at.min(body.len())]
        .char_indices()
        .rev()
        .nth(PAD)
        .map_or(0, |(i, _)| i);
    let hi_from = (at + q.len()).min(body.len());
    let hi = body[hi_from..]
        .char_indices()
        .nth(PAD)
        .map_or(body.len(), |(i, _)| hi_from + i);
    let mut s = body[lo..hi].split_whitespace().collect::<Vec<_>>().join(" ");
    if lo > 0 {
        s.insert(0, '…');
    }
    if hi < body.len() {
        s.push('…');
    }
    Some(s)
}

fn as_bool(v: Option<(Value<'_>, ObjId)>) -> bool {
    matches!(v, Some((Value::Scalar(s), _)) if matches!(s.as_ref(), ScalarValue::Boolean(true)))
}

/// `ROOT["schema"]` as an integer (0 if absent) — the migration gate.
fn read_schema<D: ReadDoc>(d: &D) -> i64 {
    as_i64(aget(d, &ROOT, "schema", At::Now))
}

// Ergonomic live-read wrappers — every existing call site stays unchanged;
// they're the `At::Now` case of the unified readers above.
fn child<D: ReadDoc>(d: &D, parent: &ObjId, key: &str) -> Option<ObjId> {
    as_obj(aget(d, parent, key, At::Now))
}
fn get_str<D: ReadDoc>(d: &D, obj: &ObjId, key: &str) -> String {
    as_str(aget(d, obj, key, At::Now))
}
fn get_i64<D: ReadDoc>(d: &D, obj: &ObjId, key: &str) -> i64 {
    as_i64(aget(d, obj, key, At::Now))
}
fn get_bool<D: ReadDoc>(d: &D, obj: &ObjId, key: &str) -> bool {
    as_bool(aget(d, obj, key, At::Now))
}
fn get_f64<D: ReadDoc>(d: &D, obj: &ObjId, key: &str) -> f64 {
    as_f64(aget(d, obj, key, At::Now))
}
fn get_folder<D: ReadDoc>(d: &D, note_obj: &ObjId) -> Vec<String> {
    folder(d, note_obj, At::Now)
}

/// Read every string element of a `List<Str>` *object* (skipping any
/// non-string element). The shared core of a note's `body`/`folder` and a
/// todo's `tags`; works live (`At::Now`) and at-heads (restore).
fn list_strs<D: ReadDoc>(d: &D, list: &ObjId, at: At) -> Vec<String> {
    (0..alen(d, list, at))
        .filter_map(|i| match aget(d, list, i, at) {
            Some((Value::Scalar(s), _)) => match s.as_ref() {
                ScalarValue::Str(s) => Some(s.to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Read a named `List<Str>` *field* (note `folder`, todo `tags`).
fn str_list<D: ReadDoc>(d: &D, obj: &ObjId, key: &str, at: At) -> Vec<String> {
    match as_obj(aget(d, obj, key, at)) {
        Some(list) => list_strs(d, &list, at),
        None => Vec::new(),
    }
}

fn folder<D: ReadDoc>(d: &D, note_obj: &ObjId, at: At) -> Vec<String> {
    str_list(d, note_obj, "folder", at)
}

fn ensure_map<T: Transactable>(tx: &mut T, key: &str) -> Result<ObjId> {
    if let Some((Value::Object(_), id)) = tx.get(ROOT, key)? {
        return Ok(id);
    }
    Ok(tx.put_object(ROOT, key, ObjType::Map)?)
}

/// Materialize one note (body included) — the single definition, used both
/// for live reads (`At::Now`) and restore's at-heads reads (`At::Heads`).
fn note_view<D: ReadDoc>(d: &D, notes: &ObjId, id: &str, at: At) -> Option<Note> {
    let obj = as_obj(aget(d, notes, id, at))?;
    Some(Note {
        id: id.to_string(),
        title: as_str(aget(d, &obj, "title", at)),
        folder: folder(d, &obj, at),
        body: as_obj(aget(d, &obj, "body", at))
            .map(|b| list_strs(d, &b, at).join("\n"))
            .unwrap_or_default(),
        created: as_i64(aget(d, &obj, "created", at)),
        modified: as_i64(aget(d, &obj, "modified", at)),
        private: as_bool(aget(d, &obj, "private", at)),
    })
}

fn all_notes<D: ReadDoc>(d: &D, notes: &ObjId, at: At) -> Vec<Note> {
    akeys(d, notes, at)
        .into_iter()
        .filter_map(|id| note_view(d, notes, &id, at))
        .collect()
}

fn all_todos<D: ReadDoc>(d: &D, todos: &ObjId, at: At) -> Vec<Todo> {
    akeys(d, todos, at)
        .into_iter()
        .filter_map(|id| {
            let o = as_obj(aget(d, todos, &id, at))?;
            Some(Todo {
                id,
                text: as_str(aget(d, &o, "text", at)),
                order: as_f64(aget(d, &o, "order", at)),
                created: as_i64(aget(d, &o, "created", at)),
                tags: str_list(d, &o, "tags", at),
            })
        })
        .collect()
}

/// Split a body string into its line elements. Empty body → no lines; a
/// trailing newline yields a trailing empty line, so `body_lines` and
/// `join("\n")` round-trip exactly.
fn body_lines(s: &str) -> Vec<String> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.split('\n').map(str::to_string).collect()
    }
}

/// Minimal `(pos, delete_count, inserted)` turning `old` lines into `new`
/// lines, or `None` if identical. Line analogue of a prefix/suffix text
/// splice: keeps unchanged leading/trailing lines so a concurrent edit to a
/// *different* line still merges. Shared by `set_note_body` and restore.
fn lines_splice(old: &[String], new: &[String]) -> Option<(usize, usize, Vec<String>)> {
    let mut p = 0;
    while p < old.len() && p < new.len() && old[p] == new[p] {
        p += 1;
    }
    let mut s = 0;
    while s < old.len() - p && s < new.len() - p && old[old.len() - 1 - s] == new[new.len() - 1 - s]
    {
        s += 1;
    }
    let del = old.len() - p - s;
    let ins = new[p..new.len() - s].to_vec();
    if del == 0 && ins.is_empty() {
        None
    } else {
        Some((p, del, ins))
    }
}

/// Delete `count` consecutive list elements starting at `pos` (each delete
/// at the same index shifts the run down).
fn delete_lines<T: Transactable>(tx: &mut T, list: &ObjId, pos: usize, count: usize) -> Result<()> {
    for _ in 0..count {
        tx.delete(list, pos)?;
    }
    Ok(())
}

/// Insert `lines` as consecutive list elements beginning at `pos`.
fn insert_lines<T: Transactable>(tx: &mut T, list: &ObjId, pos: usize, lines: &[String]) -> Result<()> {
    for (i, line) in lines.iter().enumerate() {
        tx.insert(list, pos + i, line.as_str())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the sync protocol between two stores to quiescence, the way the
    /// server loop will: each side generates while it has something to send,
    /// the other receives, repeat until both fall silent.
    fn sync(a: &mut Store, b: &mut Store) {
        let (mut sa, mut sb) = (SyncState::new(), SyncState::new());
        loop {
            let mut moved = false;
            if let Some(m) = a.generate_sync_message(&mut sa) {
                moved = true;
                b.receive_sync_message(&mut sb, &m).unwrap();
            }
            if let Some(m) = b.generate_sync_message(&mut sb) {
                moved = true;
                a.receive_sync_message(&mut sa, &m).unwrap();
            }
            if !moved {
                break;
            }
        }
    }

    #[test]
    fn two_stores_converge() {
        let dir = std::env::temp_dir().join(format!("ccal-synctest-{}", std::process::id()));
        let mut a = Store::open_at(dir.join("a.automerge")).unwrap();
        let mut b = Store::open_at(dir.join("b.automerge")).unwrap();

        let id = a
            .create_note(&["work".to_string()], "from A")
            .unwrap();
        sync(&mut a, &mut b);
        assert_eq!(b.note(&id).map(|n| n.title), Some("from A".to_string()));

        // Concurrent edits on each side, then a round trip: both converge
        // and neither edit is lost (the body edit and the new todo touch
        // different objects).
        a.set_note_body(&id, "alpha").unwrap();
        let id2 = b.add_todo("from B").unwrap();
        sync(&mut a, &mut b);
        assert_eq!(a.note(&id).map(|n| n.body), Some("alpha".to_string()));
        assert!(b.todos().iter().any(|t| t.id == id2 && t.text == "from B"));
        assert!(a.todos().iter().any(|t| t.id == id2));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_level_body_merges() {
        let dir = std::env::temp_dir().join(format!("ccal-linetest-{}", std::process::id()));
        let mut a = Store::open_at(dir.join("a.automerge")).unwrap();
        let mut b = Store::open_at(dir.join("b.automerge")).unwrap();

        // Shared three-line note.
        let id = a.create_note(&[], "n").unwrap();
        a.set_note_body(&id, "one\ntwo\nthree").unwrap();
        sync(&mut a, &mut b);
        assert_eq!(b.note(&id).unwrap().body, "one\ntwo\nthree");

        // Concurrent edits to DIFFERENT lines: a rewrites line 1, b rewrites
        // line 3. After sync both survive — the untouched lines kept their
        // identity, so this is a real merge, not last-writer-wins.
        a.set_note_body(&id, "ONE\ntwo\nthree").unwrap();
        b.set_note_body(&id, "one\ntwo\nTHREE").unwrap();
        sync(&mut a, &mut b);
        assert_eq!(a.note(&id).unwrap().body, "ONE\ntwo\nTHREE");
        assert_eq!(b.note(&id).unwrap().body, a.note(&id).unwrap().body);

        // A trailing newline round-trips (it's a trailing empty line).
        a.set_note_body(&id, "ONE\ntwo\nTHREE\n").unwrap();
        sync(&mut a, &mut b);
        assert_eq!(b.note(&id).unwrap().body, "ONE\ntwo\nTHREE\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_v1_text_body_to_line_list() {
        let dir = std::env::temp_dir().join(format!("ccal-migtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ccal.automerge");

        // Hand-build a schema-1 replica: bodies as per-character `Text`.
        {
            let actor: ActorId = "cca100000000cca100000000".parse().unwrap();
            let mut doc = Automerge::new().with_actor(actor);
            let mut tx = doc.transaction();
            tx.put(ROOT, "schema", 1i64).unwrap();
            let notes = tx.put_object(ROOT, "notes", ObjType::Map).unwrap();
            tx.put_object(ROOT, "todos", ObjType::Map).unwrap();
            let n = tx.put_object(&notes, "note-1", ObjType::Map).unwrap();
            tx.put(&n, "title", "kept").unwrap();
            tx.put(&n, "created", 111i64).unwrap();
            tx.put(&n, "modified", 222i64).unwrap();
            let body = tx.put_object(&n, "body", ObjType::Text).unwrap();
            tx.splice_text(&body, 0, 0, "line one\nline two").unwrap();
            let fl = tx.put_object(&n, "folder", ObjType::List).unwrap();
            tx.insert(&fl, 0, "work").unwrap();
            // A mark pointing at the note must survive the re-genesis.
            tx.put(ROOT, "mark/a", "note-1").unwrap();
            tx.commit_with(CommitOptions::default().with_time(0));
            std::fs::write(&path, doc.save()).unwrap();
        }

        assert_eq!(Store::file_schema(&path).unwrap(), Some(1));
        assert!(Store::needs_migration(&path).unwrap());

        // Migrate, then it is current and idempotent.
        assert!(Store::migrate_v1_in_place(&path).unwrap());
        assert!(!Store::migrate_v1_in_place(&path).unwrap(), "second run is a no-op");
        assert_eq!(Store::file_schema(&path).unwrap(), Some(SCHEMA));
        assert!(path.with_extension("automerge.v1.bak").exists(), "v1 backed up");

        // Content preserved (id, title, folder, body text, mark).
        let store = Store::open_at(&path).unwrap();
        let n = store.note("note-1").expect("note kept by id");
        assert_eq!(n.title, "kept");
        assert_eq!(n.body, "line one\nline two");
        assert_eq!(n.folder, vec!["work".to_string()]);
        assert_eq!(n.created, 111);
        assert_eq!(store.mark('a').as_deref(), Some("note-1"));

        // A fresh v2 client converges with the migrated replica.
        let mut server = Store::open_at(&path).unwrap();
        let mut client = Store::open_at(dir.join("client.automerge")).unwrap();
        sync(&mut server, &mut client);
        assert_eq!(client.note("note-1").map(|n| n.body), Some("line one\nline two".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_folder_is_recursive_and_syncs() {
        let dir = std::env::temp_dir().join(format!("ccal-foldertest-{}", std::process::id()));
        let mut a = Store::open_at(dir.join("a.automerge")).unwrap();
        let mut b = Store::open_at(dir.join("b.automerge")).unwrap();

        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let n1 = a.create_note(&s(&["work"]), "direct").unwrap();
        let n2 = a.create_note(&s(&["work", "proj"]), "deep").unwrap();
        let n3 = a.create_note(&s(&["home"]), "untouched").unwrap();

        let hit = a.rename_folder(&s(&["work"]), "job").unwrap();
        assert_eq!(hit, 2);
        assert_eq!(a.note(&n1).unwrap().folder, s(&["job"]));
        assert_eq!(a.note(&n2).unwrap().folder, s(&["job", "proj"]));
        assert_eq!(a.note(&n3).unwrap().folder, s(&["home"]));

        sync(&mut a, &mut b);
        assert_eq!(b.note(&n2).map(|n| n.folder), Some(s(&["job", "proj"])));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bulk_move_folder_tree_and_sync() {
        let dir = std::env::temp_dir().join(format!("ccal-bulktest-{}", std::process::id()));
        let mut a = Store::open_at(dir.join("a.automerge")).unwrap();
        let mut b = Store::open_at(dir.join("b.automerge")).unwrap();
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let n1 = a.create_note(&s(&["gometro"]), "g1").unwrap();
        let n2 = a.create_note(&s(&["gometro", "specs"]), "g2").unwrap();
        let n3 = a.create_note(&s(&["texecom"]), "t1").unwrap();
        let root = a.create_note(&[], "loose").unwrap();

        // folder_tree: parent prefixes appear; direct vs subtree counts.
        let tree = a.folder_tree();
        let find = |p: &[&str]| {
            tree.iter()
                .find(|(path, _, _)| *path == s(p))
                .map(|(_, d, st)| (*d, *st))
        };
        assert_eq!(find(&[]), Some((1, 1))); // the root note
        assert_eq!(find(&["gometro"]), Some((1, 2))); // 1 direct, 2 in subtree
        assert_eq!(find(&["gometro", "specs"]), Some((1, 1)));
        assert_eq!(find(&["texecom"]), Some((1, 1)));

        // move_folder re-parents the whole subtree (depth change).
        let moved = a
            .move_folder(&s(&["gometro"]), &s(&["consulting", "old", "gometro"]))
            .unwrap();
        assert_eq!(moved, 2);
        assert_eq!(a.note(&n1).unwrap().folder, s(&["consulting", "old", "gometro"]));
        assert_eq!(
            a.note(&n2).unwrap().folder,
            s(&["consulting", "old", "gometro", "specs"])
        );
        assert_eq!(a.note(&n3).unwrap().folder, s(&["texecom"])); // untouched

        // move_notes: many ids, one destination, unknown id skipped.
        let n = a
            .move_notes(
                &[n3.clone(), root.clone(), "no-such-id".to_string()],
                &s(&["archive"]),
            )
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(a.note(&n3).unwrap().folder, s(&["archive"]));
        assert_eq!(a.note(&root).unwrap().folder, s(&["archive"]));

        sync(&mut a, &mut b);
        assert_eq!(
            b.note(&n2).map(|n| n.folder),
            Some(s(&["consulting", "old", "gometro", "specs"]))
        );
        assert_eq!(b.note(&root).map(|n| n.folder), Some(s(&["archive"])));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_restores_whole_corpus_and_syncs() {
        let dir = std::env::temp_dir().join(format!("ccal-cptest-{}", std::process::id()));
        let mut a = Store::open_at(dir.join("a.automerge")).unwrap();
        let mut b = Store::open_at(dir.join("b.automerge")).unwrap();
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // Baseline corpus, then a checkpoint of exactly this state.
        let keep = a.create_note(&s(&["work"]), "keep").unwrap();
        a.set_note_body(&keep, "original body").unwrap();
        let doomed = a.create_note(&s(&["work"]), "doomed").unwrap();
        let t_keep = a.add_todo("buy milk").unwrap();
        let cp = a.create_checkpoint("before LLM reorg").unwrap();

        // A messy batch: edit, delete, create, churn todos.
        a.set_note_body(&keep, "WRECKED by the assistant").unwrap();
        a.set_note_title(&keep, "mangled").unwrap();
        a.delete_note(&doomed).unwrap();
        let added = a.create_note(&s(&["junk"]), "spurious").unwrap();
        a.set_todo_text(&t_keep, "buy oat milk instead").unwrap();
        let t_extra = a.add_todo("delete production db").unwrap();

        // Preview reports the blast radius without changing anything.
        let prev = a.preview_restore(&cp).unwrap();
        assert_eq!(prev.notes_updated, 1, "keep note differs");
        assert_eq!(prev.notes_added, 1, "doomed must be recreated");
        assert_eq!(prev.notes_deleted, 1, "spurious must be removed");
        assert_eq!(a.note(&keep).unwrap().title, "mangled", "preview is read-only");

        // Restore: the whole corpus snaps back to the checkpoint.
        let rep = a.restore_checkpoint(&cp).unwrap();
        assert_eq!((rep.notes_added, rep.notes_deleted, rep.notes_updated), (1, 1, 1));

        let n = a.note(&keep).unwrap();
        assert_eq!(n.title, "keep");
        assert_eq!(n.body, "original body");
        assert!(a.note(&doomed).is_some(), "deleted note came back");
        assert!(a.note(&added).is_none(), "post-checkpoint note removed");
        assert_eq!(a.todos().len(), 1);
        assert_eq!(a.todos()[0].text, "buy milk");
        assert!(a.todos().iter().all(|t| t.id != t_extra));

        // The restore is an ordinary forward change: it (and the
        // checkpoint metadata, lazily created) sync to a fresh peer, which
        // converges to the restored state — no special-casing needed.
        sync(&mut a, &mut b);
        assert_eq!(b.note(&keep).map(|n| n.body), Some("original body".to_string()));
        assert!(b.note(&doomed).is_some());
        assert!(b.note(&added).is_none());
        assert_eq!(b.todos().len(), 1);
        assert!(b.checkpoints().iter().any(|c| c.id == cp && c.reason == "before LLM reorg"));

        // History is intact, so the same checkpoint restores again
        // (idempotent) even after more churn.
        a.delete_note(&keep).unwrap();
        a.restore_checkpoint(&cp).unwrap();
        assert_eq!(a.note(&keep).map(|n| n.body), Some("original body".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_flag_syncs_and_is_non_retroactive_on_restore() {
        let dir = std::env::temp_dir().join(format!("ccal-privtest-{}", std::process::id()));
        let mut a = Store::open_at(dir.join("a.automerge")).unwrap();
        let mut b = Store::open_at(dir.join("b.automerge")).unwrap();
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let id = a.create_note(&s(&["secrets"]), "creds").unwrap();
        a.set_note_body(&id, "public draft").unwrap();
        assert!(!a.note(&id).unwrap().private, "notes default to not-private");

        // Checkpoint taken while the note is still public.
        let cp_public = a.create_checkpoint("before secrets").unwrap();

        // User adds a secret, THEN marks it private.
        a.set_note_body(&id, "public draft\npassword: hunter2").unwrap();
        a.set_note_private(&id, true).unwrap();
        assert!(a.note(&id).unwrap().private);

        // The flag is plain synced state.
        sync(&mut a, &mut b);
        assert!(b.note(&id).unwrap().private, "private flag converges");

        // Non-retroactive by design (user's decision): restoring to the
        // pre-private checkpoint returns that older state verbatim —
        // private goes back to false and the then-current body comes back.
        // Restore is NOT skipped for private notes.
        let rep = a.restore_checkpoint(&cp_public).unwrap();
        assert_eq!(rep.notes_updated, 1);
        let n = a.note(&id).unwrap();
        assert!(!n.private, "restore reconciles `private` like any field");
        assert_eq!(n.body, "public draft", "older snapshot body restored");

        // And a checkpoint taken WHILE private restores private=true.
        a.set_note_body(&id, "secret again").unwrap();
        a.set_note_private(&id, true).unwrap();
        let cp_priv = a.create_checkpoint("while private").unwrap();
        a.set_note_private(&id, false).unwrap();
        a.restore_checkpoint(&cp_priv).unwrap();
        assert!(a.note(&id).unwrap().private, "private state is restorable too");

        sync(&mut a, &mut b);
        assert!(b.note(&id).unwrap().private);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_respects_privacy_boundary() {
        let dir = std::env::temp_dir().join(format!("ccal-searchtest-{}", std::process::id()));
        let mut a = Store::open_at(dir.join("a.automerge")).unwrap();
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let pub_id = a.create_note(&s(&["work"]), "meeting notes").unwrap();
        a.set_note_body(&pub_id, "discussed the widget roadmap").unwrap();
        let sec_id = a.create_note(&s(&["vault"]), "bank login").unwrap();
        a.set_note_body(&sec_id, "the magicword is swordfish").unwrap();
        a.set_note_private(&sec_id, true).unwrap();

        // Body term in a public note: found.
        let h = a.search_notes("widget", false);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].id, pub_id);

        // Body term in a PRIVATE note: not found at the MCP setting…
        assert!(a.search_notes("swordfish", false).is_empty());
        // …but the user-side path (TUI) can still find it.
        assert_eq!(a.search_notes("swordfish", true).len(), 1);

        // Title/folder of a private note stay searchable either way (that
        // metadata is already visible to the assistant via list_notes).
        let by_title = a.search_notes("bank", false);
        assert_eq!(by_title.len(), 1);
        assert_eq!(by_title[0].id, sec_id);
        assert!(by_title[0].private);
        assert_eq!(a.search_notes("vault", false).len(), 1);

        // Empty query never spams the whole corpus.
        assert!(a.search_notes("   ", false).is_empty());

        // Snippets: a body hit yields a one-line context window; a
        // title/folder-only hit and a private body hit yield none.
        let body_hit = a.search_notes_snippets("widget", false);
        assert_eq!(body_hit.len(), 1);
        let snip = body_hit[0].1.as_deref().unwrap();
        assert!(snip.contains("widget roadmap"));
        assert!(!snip.contains('\n'), "snippet is single-line");

        // Matched on title only → no snippet (the title is already returned).
        let title_hit = a.search_notes_snippets("meeting", false);
        assert_eq!(title_hit.len(), 1);
        assert!(title_hit[0].1.is_none());

        // Private note matched on title → still no body snippet, ever.
        let priv_hit = a.search_notes_snippets("bank", false);
        assert_eq!(priv_hit.len(), 1);
        assert!(priv_hit[0].0.private);
        assert!(priv_hit[0].1.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn calendar_subscriptions_sync_without_genesis_change() {
        let dir = std::env::temp_dir().join(format!("ccal-caltest-{}", std::process::id()));
        let mut a = Store::open_at(dir.join("a.automerge")).unwrap();
        let mut b = Store::open_at(dir.join("b.automerge")).unwrap();

        // Two replicas each add a calendar *before* ever syncing — the
        // exact concurrent-seed scenario that breaks a shared container
        // map. Unique `cal/<uuid>` keys mean both survive the merge.
        let ca = a.add_calendar("https://a.example/x.ics", "Work").unwrap();
        let cb = b.add_calendar("https://b.example/y.ics", "").unwrap();
        sync(&mut a, &mut b);

        for s in [&a, &b] {
            let cals = s.calendars();
            assert_eq!(cals.len(), 2, "both subscriptions converge");
            assert!(cals.iter().any(|c| c.id == ca && c.url == "https://a.example/x.ics"));
            assert!(cals.iter().any(|c| c.id == cb));
        }

        // Name backfill (what the fetch thread does after reading
        // X-WR-CALNAME) and removal both ride sync like any edit.
        b.set_calendar_name(&cb, "Personal").unwrap();
        a.remove_calendar(&ca).unwrap();
        sync(&mut a, &mut b);
        let cals = b.calendars();
        assert_eq!(cals.len(), 1);
        assert_eq!(cals[0].id, cb);
        assert_eq!(cals[0].name, "Personal");
        assert!(a.calendars().iter().all(|c| c.id != ca), "removal converged");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_lists_changes_and_time_travels() {
        let dir = std::env::temp_dir().join(format!("ccal-histtest-{}", std::process::id()));
        let mut a = Store::open_at(dir.join("a.automerge")).unwrap();
        let mut b = Store::open_at(dir.join("b.automerge")).unwrap();
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let aid = a.create_note(&s(&["w"]), "alpha").unwrap();
        a.set_note_body(&aid, "one").unwrap();
        // Snapshot of state S1 (alpha="one", no other notes).
        let s1 = a.history()[0].hash.clone();
        let cp = a.create_checkpoint("at S1").unwrap();

        // Move on: a second note, and a body rewrite.
        let bid = a.create_note(&s(&["w"]), "beta").unwrap();
        a.set_note_body(&aid, "two").unwrap();

        let h = a.history();
        // Newest first, and strictly growing as we edit.
        assert!(h.len() >= 5, "every commit is a change: {}", h.len());
        // The checkpoint's head shows up inline as a named row.
        assert!(
            h.iter().any(|r| r.checkpoint.as_deref() == Some("at S1")),
            "checkpoint tagged in timeline"
        );
        // Interactive commits are timestamped (history has a real clock).
        assert!(h[0].ts > 0, "newest change carries a wall-clock time");

        // Time-travel straight to a raw change (not a checkpoint): the
        // whole corpus snaps back to S1.
        let rep = a.restore_to(&s1).unwrap();
        assert_eq!(rep.notes_deleted, 1); // beta removed
        assert_eq!(a.note(&aid).unwrap().body, "one");
        assert!(a.note(&bid).is_none());

        // History is append-only — restore added a change, didn't erase any.
        assert!(a.history().len() > h.len());
        // …and the checkpoint is still restorable too (same end state here).
        a.restore_checkpoint(&cp).unwrap();
        assert_eq!(a.note(&aid).unwrap().body, "one");

        // It's just another change: a fresh peer converges to the result.
        sync(&mut a, &mut b);
        assert_eq!(b.note(&aid).map(|n| n.body), Some("one".to_string()));
        assert!(b.note(&bid).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
