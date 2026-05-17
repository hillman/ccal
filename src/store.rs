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
use automerge::transaction::Transactable;
use automerge::{Automerge, ObjId, ObjType, ReadDoc, ScalarValue, Value, ROOT};
use std::path::PathBuf;

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

impl Store {
    /// Open the store from disk, creating an empty document if absent.
    pub fn open() -> Result<Self> {
        let path = data_path()?;
        let mut doc = if path.exists() {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            Automerge::load(&bytes).context("parsing Automerge document")?
        } else {
            Automerge::new()
        };

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
