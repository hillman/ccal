//! TUI state and key handling. Talks only to `ccal::store` / `ccal::models`.
//! The notes "folders" are a virtual tree derived from each note's
//! `folder` array — there is no folder entity and no filesystem.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Result;
use edtui::{EditorEventHandler, EditorMode, EditorState, Lines};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use ccal::models::{now_ms, Note, NoteMeta, Todo};
use ccal::store::Store;

use crate::sync_client;

#[derive(PartialEq, Clone, Copy)]
pub enum Tab {
    Todos,
    Notes,
}

pub enum Prompt {
    AddTodo,
    EditTodo(String),
    NewNote,
    RenameNote(String),
    /// Move a note: buffer is a slash-separated folder path ("" = root).
    MoveNote(String),
    /// Rename the folder at this full path; buffer edits its last component.
    RenameFolder(Vec<String>),
}

pub enum Mode {
    Normal,
    Input { prompt: Prompt, buffer: String },
    NoteEdit { id: String, title: String },
    /// Live text search across *all* notes (the folder view collapses to a
    /// flat list of matches). Esc returns to Normal.
    Search { query: String },
}

#[derive(Clone)]
pub enum Entry {
    Dir(String),
    Note { id: String, title: String, private: bool },
}

pub struct App {
    /// Shared with the background sync thread (if any). The lock is held
    /// only for individual store calls — never across a redraw or IO.
    store: Arc<Mutex<Store>>,
    /// `None` in standalone mode (no `CCAL_SYNC_URL`) — the rest of the app
    /// is byte-for-byte the same code path either way.
    sync: Option<sync_client::Handle>,
    /// Last sync status string surfaced to the bar, so a steady connection
    /// state doesn't keep stomping transient action messages.
    last_sync: String,
    pub tab: Tab,
    pub mode: Mode,
    pub should_quit: bool,
    pub status: String,

    pub todos: Vec<Todo>,
    pub todo_sel: usize,

    /// Current notes folder path ([] = root).
    pub cur: Vec<String>,
    pub entries: Vec<Entry>,
    pub entry_sel: usize,
    pub editor: EditorState,
    /// Persisted across keystrokes — holds multi-key Vim state (e.g. `dd`).
    edit_events: EditorEventHandler,
    /// Materialized once when entering [`Mode::Search`] (and refreshed on a
    /// live sync change) so per-keystroke filtering never re-reads every
    /// note body. Empty/ignored outside search.
    search_index: Vec<Note>,
}

impl App {
    pub fn new() -> Result<Self> {
        let store = Arc::new(Mutex::new(Store::open()?));

        // Standalone unless both a URL and a token resolve (env var or
        // config file, env winning). No URL/token → no sync thread, same
        // code path otherwise.
        let cfg = ccal::Config::load()?;
        let sync = match (cfg.client_url(), cfg.client_token()) {
            (Some(url), Some(token)) => {
                Some(sync_client::spawn(store.clone(), url, token))
            }
            _ => None,
        };

        let mut app = Self {
            store,
            sync,
            last_sync: String::new(),
            tab: Tab::Todos,
            mode: Mode::Normal,
            should_quit: false,
            status: "Tab: switch · q: quit".into(),
            todos: Vec::new(),
            todo_sel: 0,
            cur: Vec::new(),
            entries: Vec::new(),
            entry_sel: 0,
            editor: EditorState::new(Lines::default()),
            edit_events: EditorEventHandler::default(),
            search_index: Vec::new(),
        };
        app.refresh();
        Ok(app)
    }

    /// Brief lock on the shared store. Every call site takes it for one
    /// store operation and drops it on the same statement — the sync thread
    /// must never wait behind the UI.
    fn st(&self) -> MutexGuard<'_, Store> {
        self.store.lock().expect("store mutex poisoned")
    }

    fn persist(&mut self) {
        let r = self.st().save();
        if let Err(e) = r {
            self.status = format!("Save failed: {e}");
        }
    }

    /// Called once per UI loop: fold in anything the background sync thread
    /// merged, and surface connection state without clobbering transient
    /// action messages.
    pub fn tick(&mut self) {
        let Some(sync) = &self.sync else { return };
        let dirty = sync.dirty.clone();
        let status_arc = sync.status.clone();

        let remote_changed = dirty.swap(false, Ordering::SeqCst);
        if remote_changed {
            // Rebuild the lists. Deliberately does NOT touch `editor`: a
            // remote edit to the note you're typing in still merges in the
            // doc (char-level Text CRDT) and is reconciled on next open;
            // yanking the buffer mid-keystroke would be worse.
            self.refresh();
        }

        let cur = status_arc.lock().map(|s| s.clone()).unwrap_or_default();
        let editing = !matches!(self.mode, Mode::Normal);
        if cur != self.last_sync {
            self.last_sync = cur.clone();
            if !editing {
                self.status = cur;
            }
        } else if remote_changed && !editing {
            self.status = "Synced".into();
        }
    }

    /// Persistent header indicator, independent of the transient status
    /// line: connection dot + how long ago we last exchanged with the peer.
    /// `None` in standalone mode (no sync configured) so the header stays
    /// clean.
    pub fn sync_indicator(&self) -> Option<String> {
        let sync = self.sync.as_ref()?;
        let connected = sync.connected.load(Ordering::SeqCst);
        let last = sync.last_sync.load(Ordering::SeqCst);
        let when = if last == 0 {
            "never".to_string()
        } else {
            ago(now_ms() - last)
        };
        Some(if connected {
            if last == 0 {
                "● online · handshaking…".to_string()
            } else {
                format!("● online · synced {when}")
            }
        } else {
            format!("○ offline · synced {when}")
        })
    }

    fn refresh(&mut self) {
        let todos = self.st().todos();
        self.todos = todos;
        // Keep the search corpus live: a remote sync edit during a search
        // should show up in the results too.
        if matches!(self.mode, Mode::Search { .. }) {
            let notes = self.st().notes();
            self.search_index = notes;
        }
        self.entries = self.build_entries();
        self.clamp();
    }

    /// Reload the document from disk (picks up an external `import-bear`).
    /// Replaces the *inner* doc so the sync thread keeps the same shared
    /// handle. Note: doing this while sync is live makes the peer resync
    /// from scratch — fine for the import-bear use case, just not free.
    fn reload(&mut self) {
        match Store::open() {
            Ok(s) => {
                *self.st() = s;
                self.refresh();
                self.status = "Reloaded from disk".into();
            }
            Err(e) => self.status = format!("Reload failed: {e}"),
        }
    }

    fn build_entries(&self) -> Vec<Entry> {
        // Search collapses the folder tree to a flat list of matches across
        // the whole corpus, filtered live from the in-memory index (no
        // store lock, no per-keystroke body re-read).
        if let Mode::Search { query } = &self.mode {
            let q = query.trim().to_lowercase();
            if q.is_empty() {
                return Vec::new();
            }
            let mut hits: Vec<&Note> = self
                .search_index
                .iter()
                .filter(|n| {
                    n.title.to_lowercase().contains(&q)
                        || n.folder.join("/").to_lowercase().contains(&q)
                        || n.body.to_lowercase().contains(&q)
                })
                .collect();
            hits.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.title.cmp(&b.title)));
            return hits
                .into_iter()
                .map(|n| {
                    let where_ = if n.folder.is_empty() {
                        "/".to_string()
                    } else {
                        format!("/{}", n.folder.join("/"))
                    };
                    Entry::Note {
                        id: n.id.clone(),
                        title: format!(
                            "{}  ⟨{where_}⟩",
                            if n.title.is_empty() { "(untitled)" } else { &n.title }
                        ),
                        private: n.private,
                    }
                })
                .collect();
        }

        let notes = self.st().note_metas();
        let depth = self.cur.len();
        let mut dirs: Vec<String> = Vec::new();
        let mut here: Vec<&NoteMeta> = Vec::new();
        for n in &notes {
            if n.folder.len() == depth && n.folder == self.cur {
                here.push(n);
            } else if n.folder.len() > depth && n.folder[..depth] == self.cur[..] {
                let name = n.folder[depth].clone();
                if !dirs.contains(&name) {
                    dirs.push(name);
                }
            }
        }
        dirs.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
        here.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.title.cmp(&b.title)));

        let mut out: Vec<Entry> = dirs.into_iter().map(Entry::Dir).collect();
        out.extend(here.into_iter().map(|n| Entry::Note {
            id: n.id.clone(),
            title: if n.title.is_empty() { "(untitled)".into() } else { n.title.clone() },
            private: n.private,
        }));
        out
    }

    fn at_root(&self) -> bool {
        self.cur.is_empty()
    }
    /// A flat list (no leading ".." row): the root folder, or any search
    /// result set (matches span folders, so there's nothing to go "up" to).
    pub fn flat_list(&self) -> bool {
        self.at_root() || matches!(self.mode, Mode::Search { .. })
    }
    fn note_rows(&self) -> usize {
        self.entries.len() + if self.flat_list() { 0 } else { 1 }
    }

    fn clamp(&mut self) {
        if self.todo_sel >= self.todos.len() {
            self.todo_sel = self.todos.len().saturating_sub(1);
        }
        let rows = self.note_rows();
        if self.entry_sel >= rows {
            self.entry_sel = rows.saturating_sub(1);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        match &self.mode {
            Mode::Normal => self.normal_key(key)?,
            Mode::Input { .. } => self.input_key(key)?,
            Mode::NoteEdit { .. } => self.editor_key(key)?,
            Mode::Search { .. } => self.search_key(key)?,
        }
        self.clamp();
        Ok(())
    }

    // ---- Normal --------------------------------------------------------

    fn normal_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.tab = match self.tab {
                    Tab::Todos => Tab::Notes,
                    Tab::Notes => Tab::Todos,
                };
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            _ => match self.tab {
                Tab::Todos => self.todos_key(key)?,
                Tab::Notes => self.notes_key(key)?,
            },
        }
        Ok(())
    }

    fn move_sel(&mut self, delta: isize) {
        let rows = self.note_rows();
        let (sel, len) = match self.tab {
            Tab::Todos => (&mut self.todo_sel, self.todos.len()),
            Tab::Notes => (&mut self.entry_sel, rows),
        };
        if len == 0 {
            return;
        }
        *sel = ((*sel as isize + delta).rem_euclid(len as isize)) as usize;
    }

    fn todos_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('a') => {
                self.mode = Mode::Input { prompt: Prompt::AddTodo, buffer: String::new() };
                self.status = "New todo — Enter: save · Esc: cancel".into();
            }
            KeyCode::Char('e') | KeyCode::Enter if !self.todos.is_empty() => {
                let t = &self.todos[self.todo_sel];
                self.mode = Mode::Input {
                    prompt: Prompt::EditTodo(t.id.clone()),
                    buffer: t.text.clone(),
                };
                self.status = "Edit todo — Enter: save · Esc: cancel".into();
            }
            KeyCode::Char('d') if !self.todos.is_empty() => {
                let id = self.todos[self.todo_sel].id.clone();
                self.st().delete_todo(&id)?;
                self.persist();
                self.refresh();
                self.status = "Todo deleted".into();
            }
            KeyCode::Char('J') if !self.todos.is_empty() => {
                let i = self.todo_sel;
                if i + 1 < self.todos.len() {
                    let (a, b) = (self.todos[i].id.clone(), self.todos[i + 1].id.clone());
                    self.st().swap_todo_order(&a, &b)?;
                    self.persist();
                    self.refresh();
                    self.todo_sel = i + 1;
                }
            }
            KeyCode::Char('K') if !self.todos.is_empty() => {
                let i = self.todo_sel;
                if i > 0 {
                    let (a, b) = (self.todos[i].id.clone(), self.todos[i - 1].id.clone());
                    self.st().swap_todo_order(&a, &b)?;
                    self.persist();
                    self.refresh();
                    self.todo_sel = i - 1;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// `None` = the ".." row; otherwise the selected entry.
    fn selected(&self) -> Option<&Entry> {
        if self.flat_list() {
            self.entries.get(self.entry_sel)
        } else if self.entry_sel == 0 {
            None
        } else {
            self.entries.get(self.entry_sel - 1)
        }
    }

    fn notes_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('n') => {
                self.mode = Mode::Input { prompt: Prompt::NewNote, buffer: String::new() };
                self.status = "Note name — Enter: create · Esc: cancel".into();
            }
            KeyCode::Enter | KeyCode::Right => match self.selected().cloned() {
                None => {
                    self.cur.pop();
                    self.entry_sel = 0;
                    self.entries = self.build_entries();
                }
                Some(Entry::Dir(name)) => {
                    self.cur.push(name);
                    self.entry_sel = 0;
                    self.entries = self.build_entries();
                }
                Some(Entry::Note { id, title, .. }) => {
                    let body = self.st().note(&id).map(|n| n.body).unwrap_or_default();
                    self.editor = make_editor(&body);
                    self.mode = Mode::NoteEdit { id, title };
                    self.status = "Editing (NORMAL) — i insert · q save & close".into();
                }
            },
            KeyCode::Left | KeyCode::Char('h') if !self.at_root() => {
                self.cur.pop();
                self.entry_sel = 0;
                self.entries = self.build_entries();
            }
            KeyCode::Char('R') => match self.selected().cloned() {
                Some(Entry::Note { id, title, .. }) => {
                    self.mode = Mode::Input {
                        prompt: Prompt::RenameNote(id),
                        buffer: title,
                    };
                    self.status = "Rename note — Enter: save · Esc: cancel".into();
                }
                Some(Entry::Dir(name)) => {
                    let mut path = self.cur.clone();
                    path.push(name.clone());
                    self.mode = Mode::Input {
                        prompt: Prompt::RenameFolder(path),
                        buffer: name,
                    };
                    self.status = "Rename folder (whole subtree) — Enter · Esc".into();
                }
                None => {}
            },
            KeyCode::Char('m') => {
                if let Some(Entry::Note { id, .. }) = self.selected().cloned() {
                    self.mode = Mode::Input {
                        prompt: Prompt::MoveNote(id),
                        buffer: self.cur.join("/"),
                    };
                    self.status = "Move to folder (a/b, blank = root) — Enter · Esc".into();
                } else {
                    self.status = "Select a note to move".into();
                }
            }
            KeyCode::Char('d') => {
                if let Some(Entry::Note { id, title, .. }) = self.selected().cloned() {
                    self.st().delete_note(&id)?;
                    self.persist();
                    self.entries = self.build_entries();
                    self.clamp();
                    self.status = format!("Deleted note '{title}'");
                } else if matches!(self.selected(), Some(Entry::Dir(_))) {
                    self.status = "Folders are derived — delete the notes inside".into();
                }
            }
            KeyCode::Char('p') => {
                if let Some(Entry::Note { id, title, private }) = self.selected().cloned() {
                    self.st().set_note_private(&id, !private)?;
                    self.persist();
                    self.entries = self.build_entries();
                    self.status = if private {
                        format!("'{title}' is no longer private — visible to assistants")
                    } else {
                        // Nudge: privacy starts now; it can't hide content
                        // that older checkpoints/history already captured.
                        format!(
                            "'{title}' is now private (hidden from assistants). \
                             Tip: mark notes private BEFORE adding secrets — \
                             earlier snapshots still hold prior content."
                        )
                    };
                } else {
                    self.status = "Select a note to toggle private".into();
                }
            }
            KeyCode::Char('/') => self.enter_search(),
            _ => {}
        }
        Ok(())
    }

    // ---- Search --------------------------------------------------------

    fn enter_search(&mut self) {
        // Materialize the corpus once; per-keystroke filtering is then a
        // pure in-memory scan (see `build_entries`).
        let notes = self.st().notes();
        self.search_index = notes;
        self.mode = Mode::Search { query: String::new() };
        self.entry_sel = 0;
        self.entries = self.build_entries();
        self.status = "Search all notes — type to filter · ↑↓ select · Enter open · Esc cancel".into();
    }

    fn exit_search(&mut self) {
        self.search_index = Vec::new();
        self.mode = Mode::Normal;
        self.entry_sel = 0;
        self.entries = self.build_entries();
        self.status = "Search cancelled".into();
    }

    fn search_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.exit_search(),
            KeyCode::Down => self.move_sel(1),
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Enter => {
                if let Some(Entry::Note { id, .. }) = self.selected().cloned() {
                    let note = self.st().note(&id);
                    let title = note.as_ref().map(|n| n.title.clone()).unwrap_or_default();
                    let body = note.map(|n| n.body).unwrap_or_default();
                    self.search_index = Vec::new();
                    self.editor = make_editor(&body);
                    self.mode = Mode::NoteEdit { id, title };
                    self.status = "Editing (NORMAL) — i insert · q save & close".into();
                }
            }
            KeyCode::Backspace => {
                if let Mode::Search { query } = &mut self.mode {
                    query.pop();
                }
                self.entry_sel = 0;
                self.entries = self.build_entries();
            }
            KeyCode::Char(c) => {
                if let Mode::Search { query } = &mut self.mode {
                    query.push(c);
                }
                self.entry_sel = 0;
                self.entries = self.build_entries();
            }
            _ => {}
        }
        Ok(())
    }

    // ---- Input ---------------------------------------------------------

    fn input_key(&mut self, key: KeyEvent) -> Result<()> {
        let Mode::Input { prompt, buffer } = &mut self.mode else { return Ok(()) };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = "Cancelled".into();
            }
            KeyCode::Char(c) => buffer.push(c),
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Enter => {
                let text = buffer.trim().to_string();
                match prompt {
                    Prompt::AddTodo => {
                        if !text.is_empty() {
                            self.st().add_todo(&text)?;
                            self.persist();
                            self.refresh();
                            self.todo_sel = self.todos.len().saturating_sub(1);
                        }
                        self.mode = Mode::Normal;
                        self.status = "Todo added".into();
                    }
                    Prompt::EditTodo(id) => {
                        let id = id.clone();
                        if !text.is_empty() {
                            self.st().set_todo_text(&id, &text)?;
                            self.persist();
                            self.refresh();
                        }
                        self.mode = Mode::Normal;
                        self.status = "Todo updated".into();
                    }
                    Prompt::RenameNote(id) => {
                        let id = id.clone();
                        if !text.is_empty() {
                            self.st().set_note_title(&id, &text)?;
                            self.persist();
                            self.refresh();
                        }
                        self.mode = Mode::Normal;
                        self.status = "Note renamed".into();
                    }
                    Prompt::MoveNote(id) => {
                        let id = id.clone();
                        let folder = parse_path(&text);
                        self.st().set_note_folder(&id, &folder)?;
                        self.persist();
                        self.refresh();
                        self.mode = Mode::Normal;
                        self.status = if folder.is_empty() {
                            "Moved to root".into()
                        } else {
                            format!("Moved to /{}", folder.join("/"))
                        };
                    }
                    Prompt::RenameFolder(path) => {
                        let path = path.clone();
                        let name = text;
                        if name.is_empty() || name.contains('/') {
                            self.mode = Mode::Normal;
                            self.status = "Folder name must be one path component".into();
                        } else {
                            let n = self.st().rename_folder(&path, &name)?;
                            self.persist();
                            self.refresh();
                            self.mode = Mode::Normal;
                            self.status = format!("Folder renamed ({n} notes updated)");
                        }
                    }
                    Prompt::NewNote => {
                        // A `folder/note` (or `folder\note`) name files the
                        // note: the last segment is the title, anything
                        // before it is a folder path relative to the folder
                        // you're in. Typing a fresh path *is* how you make a
                        // folder.
                        let mut parts = parse_path(&text);
                        let title = parts.pop().unwrap_or_default();
                        if title.is_empty() {
                            self.mode = Mode::Normal;
                            self.status = "Empty name — cancelled".into();
                        } else {
                            let mut folder = self.cur.clone();
                            folder.extend(parts);
                            let id = self.st().create_note(&folder, &title)?;
                            self.persist();
                            self.entries = self.build_entries();
                            self.editor = make_editor("");
                            // New note: drop straight into Insert so you
                            // can type immediately.
                            self.editor.mode = EditorMode::Insert;
                            let at = if folder.is_empty() {
                                String::new()
                            } else {
                                format!(" in /{}", folder.join("/"))
                            };
                            self.mode = Mode::NoteEdit { id, title };
                            self.status =
                                format!("Editing (INSERT){at} — Esc then q to save & close");
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ---- Note editor ---------------------------------------------------

    fn save_body(&mut self) -> Result<()> {
        if let Mode::NoteEdit { id, .. } = &self.mode {
            let id = id.clone();
            let content = self.editor.lines.to_string();
            self.st().set_note_body(&id, &content)?;
            self.persist();
        }
        Ok(())
    }

    /// Modal routing: app commands only fire in the editor's Normal mode;
    /// Insert/Visual/Search keystrokes go straight to edtui. This is why
    /// there is no key conflict — typing never triggers app actions.
    fn editor_key(&mut self, key: KeyEvent) -> Result<()> {
        // Ctrl+S saves without leaving the editor, in any mode.
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.save_body()?;
            self.status = "Saved".into();
            return Ok(());
        }
        // In Normal mode, Esc / q save and return to the note list. In
        // Insert mode Esc belongs to edtui (Insert -> Normal), so it is
        // deliberately not intercepted here.
        if self.editor.mode == EditorMode::Normal
            && matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
        {
            self.save_body()?;
            self.mode = Mode::Normal;
            self.entries = self.build_entries();
            self.clamp();
            self.status = "Saved · back to list".into();
            return Ok(());
        }
        self.edit_events.on_key_event(key, &mut self.editor);
        Ok(())
    }
}

/// Folder path → components. Either separator (`/` or `\`) splits, blank
/// segments and surrounding whitespace are dropped, so `""`, `"/"`, and
/// `" a / b "` behave sanely (`[]`, `[]`, `["a","b"]`).
fn parse_path(s: &str) -> Vec<String> {
    s.split(['/', '\\'])
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .map(|c| c.to_string())
        .collect()
}

/// Coarse human "N ago" for a millisecond duration (negative clamps to 0).
fn ago(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    if s < 5 {
        "just now".to_string()
    } else if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}

fn make_editor(content: &str) -> EditorState {
    EditorState::new(Lines::from(content))
}
