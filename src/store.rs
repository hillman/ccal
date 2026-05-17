//! The single source of truth: one Automerge document, persisted as one
//! binary blob in the data dir. This module is the *only* place that knows
//! Automerge exists; callers work entirely in [`crate::models`] terms.
//!
//! Built on the low-level `Automerge` API with explicit transactions: each
//! interactive edit is its own small transaction, and bulk import is a
//! single transaction over all notes (doing it per-note through the
//! convenience `AutoCommit` wrapper is pathologically slow at scale).
//!
//! `body` is a real Automerge `Text` object, so concurrent edits to the
//! same note merge at character granularity. Interactive edits splice only
//! the changed prefix/suffix region; import inserts the whole body in one op.
//!
//! Document layout (ROOT map):
//! - `schema`: Int
//! - `notes`:  Map  id -> { title:Str, folder:List<Str>, body:Text,
//!                           created:Int, modified:Int }
//! - `todos`:  Map  id -> { text:Str, order:F64, created:Int }

use anyhow::{anyhow, Context, Result};
use automerge::sync::{Message as AmSyncMessage, SyncDoc};
use automerge::transaction::{CommitOptions, Transactable};
use automerge::{ActorId, Automerge, ObjId, ObjType, ReadDoc, ScalarValue, Value, ROOT};
use std::path::{Path, PathBuf};

/// Per-peer sync state for the Automerge sync protocol. Opaque to callers;
/// create one with [`SyncState::default`] per connected peer and keep it for
/// the life of that connection.
pub use automerge::sync::State as SyncState;

use crate::models::{new_id, now_ms, Note, NoteInput, NoteMeta, Todo};

const SCHEMA: i64 = 1;

pub struct Store {
    doc: Automerge,
    path: PathBuf,
    notes: ObjId,
    todos: ObjId,
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
        tx.commit();

        Ok(Self { doc, path, notes, todos })
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
                })
            })
            .collect()
    }

    fn read_note(&self, id: &str) -> Option<Note> {
        let obj = child(&self.doc, &self.notes, id)?;
        Some(Note {
            id: id.to_string(),
            title: get_str(&self.doc, &obj, "title"),
            folder: get_folder(&self.doc, &obj),
            body: child(&self.doc, &obj, "body")
                .and_then(|b| self.doc.text(&b).ok())
                .unwrap_or_default(),
            created: get_i64(&self.doc, &obj, "created"),
            modified: get_i64(&self.doc, &obj, "modified"),
        })
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
        tx.put_object(&obj, "body", ObjType::Text)?;
        let list = tx.put_object(&obj, "folder", ObjType::List)?;
        for (i, c) in folder.iter().enumerate() {
            tx.insert(&list, i, c.as_str())?;
        }
        tx.commit();
        Ok(id)
    }

    /// Bulk-import notes in a single transaction (the only fast path at
    /// scale). The whole body goes in as one splice op per note.
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
            let body = tx.put_object(&obj, "body", ObjType::Text)?;
            if !n.body.is_empty() {
                tx.splice_text(&body, 0, 0, &n.body)?;
            }
            let list = tx.put_object(&obj, "folder", ObjType::List)?;
            for (i, c) in n.folder.iter().enumerate() {
                tx.insert(&list, i, c.as_str())?;
            }
            progress(idx + 1);
        }
        tx.commit();
        Ok(items.len())
    }

    pub fn set_note_title(&mut self, id: &str, title: &str) -> Result<()> {
        let obj = child(&self.doc, &self.notes, id).ok_or_else(|| anyhow!("no such note"))?;
        let mut tx = self.doc.transaction();
        tx.put(&obj, "title", title)?;
        tx.put(&obj, "modified", now_ms())?;
        tx.commit();
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
        tx.commit();
        Ok(())
    }

    /// Update the note body, splicing only the changed region so concurrent
    /// edits merge at character granularity.
    pub fn set_note_body(&mut self, id: &str, new: &str) -> Result<()> {
        let obj = child(&self.doc, &self.notes, id).ok_or_else(|| anyhow!("no such note"))?;
        let body = child(&self.doc, &obj, "body")
            .ok_or_else(|| anyhow!("note has no body object"))?;

        let old: Vec<char> = self.doc.text(&body)?.chars().collect();
        let newc: Vec<char> = new.chars().collect();

        let mut p = 0;
        while p < old.len() && p < newc.len() && old[p] == newc[p] {
            p += 1;
        }
        let mut s = 0;
        while s < old.len() - p
            && s < newc.len() - p
            && old[old.len() - 1 - s] == newc[newc.len() - 1 - s]
        {
            s += 1;
        }
        let del = old.len() - p - s;
        let ins: String = newc[p..newc.len() - s].iter().collect();
        if del == 0 && ins.is_empty() {
            return Ok(());
        }
        let mut tx = self.doc.transaction();
        tx.splice_text(&body, p, del as isize, &ins)?;
        tx.put(&obj, "modified", now_ms())?;
        tx.commit();
        Ok(())
    }

    pub fn delete_note(&mut self, id: &str) -> Result<()> {
        let mut tx = self.doc.transaction();
        tx.delete(&self.notes, id)?;
        tx.commit();
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
        tx.commit();
        Ok(id)
    }

    pub fn set_todo_text(&mut self, id: &str, text: &str) -> Result<()> {
        let obj = child(&self.doc, &self.todos, id).ok_or_else(|| anyhow!("no such todo"))?;
        let mut tx = self.doc.transaction();
        tx.put(&obj, "text", text)?;
        tx.commit();
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
        tx.commit();
        Ok(())
    }

    pub fn delete_todo(&mut self, id: &str) -> Result<()> {
        let mut tx = self.doc.transaction();
        tx.delete(&self.todos, id)?;
        tx.commit();
        Ok(())
    }
}

// ---- Free read helpers (work against any ReadDoc) ----------------------

fn child<D: ReadDoc>(d: &D, parent: &ObjId, key: &str) -> Option<ObjId> {
    match d.get(parent, key) {
        Ok(Some((Value::Object(_), id))) => Some(id),
        _ => None,
    }
}

fn get_str<D: ReadDoc>(d: &D, obj: &ObjId, key: &str) -> String {
    match d.get(obj, key) {
        Ok(Some((Value::Scalar(s), _))) => match s.as_ref() {
            ScalarValue::Str(s) => s.to_string(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn get_i64<D: ReadDoc>(d: &D, obj: &ObjId, key: &str) -> i64 {
    match d.get(obj, key) {
        Ok(Some((Value::Scalar(s), _))) => match s.as_ref() {
            ScalarValue::Int(i) => *i,
            ScalarValue::Uint(u) => *u as i64,
            ScalarValue::Timestamp(t) => *t,
            _ => 0,
        },
        _ => 0,
    }
}

fn get_f64<D: ReadDoc>(d: &D, obj: &ObjId, key: &str) -> f64 {
    match d.get(obj, key) {
        Ok(Some((Value::Scalar(s), _))) => match s.as_ref() {
            ScalarValue::F64(f) => *f,
            ScalarValue::Int(i) => *i as f64,
            _ => 0.0,
        },
        _ => 0.0,
    }
}

fn get_folder<D: ReadDoc>(d: &D, note_obj: &ObjId) -> Vec<String> {
    let Some(list) = child(d, note_obj, "folder") else {
        return Vec::new();
    };
    (0..d.length(&list))
        .filter_map(|i| match d.get(&list, i) {
            Ok(Some((Value::Scalar(s), _))) => match s.as_ref() {
                ScalarValue::Str(s) => Some(s.to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn ensure_map<T: Transactable>(tx: &mut T, key: &str) -> Result<ObjId> {
    if let Some((Value::Object(_), id)) = tx.get(ROOT, key)? {
        return Ok(id);
    }
    Ok(tx.put_object(ROOT, key, ObjType::Map)?)
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
        // and neither edit is lost (char-level Text merge).
        a.set_note_body(&id, "alpha").unwrap();
        let id2 = b.add_todo("from B").unwrap();
        sync(&mut a, &mut b);
        assert_eq!(a.note(&id).map(|n| n.body), Some("alpha".to_string()));
        assert!(b.todos().iter().any(|t| t.id == id2 && t.text == "from B"));
        assert!(a.todos().iter().any(|t| t.id == id2));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
