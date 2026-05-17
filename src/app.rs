//! TUI state and key handling. Talks only to `ccal::store` / `ccal::models`.
//! The notes "folders" are a virtual tree derived from each note's
//! `folder` array — there is no folder entity and no filesystem.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Result;
use edtui::{EditorEventHandler, EditorMode, EditorState, Lines};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use ccal::calendar::Occurrence;
use ccal::models::{now_ms, Calendar, HistoryRow, Note, NoteMeta, Todo};
use ccal::store::Store;

use crate::cal_sync::{self, CalStatus};
use crate::sync_client;

#[derive(PartialEq, Clone, Copy)]
pub enum Tab {
    Todos,
    Notes,
    Calendar,
    History,
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
    /// Create a named checkpoint; buffer is the reason text.
    NewCheckpoint,
    /// Subscribe to a calendar; buffer is the pasted ICS URL.
    AddCalendar,
}

pub enum Mode {
    Normal,
    Input { prompt: Prompt, buffer: String },
    NoteEdit { id: String, title: String },
    /// Live text search across *all* notes (the folder view collapses to a
    /// flat list of matches). Esc returns to Normal.
    Search { query: String },
}

/// Half-finished `g`-prefixed bookmark chord, awaiting its next key. Only
/// ever set in [`Mode::Normal`]; consumed by the next keystroke so it can
/// never linger across a mode change.
#[derive(Clone, Copy)]
pub enum Pending {
    /// `g` seen — next key is either `m` (→ [`Pending::SetMark`]) or the
    /// bookmark to jump to.
    Goto,
    /// `gm` seen — next key names the bookmark to point at the selected note.
    SetMark,
}

#[derive(Clone)]
pub enum Entry {
    Dir(String),
    Note { id: String, title: String, private: bool },
}

/// What the right-hand pane of the Notes tab shows for the current
/// selection. Recomputed (cheaply) after any selection/navigation change
/// so `ui` stays a pure reader. Only ever populated on the Notes tab in
/// Normal mode — search/edit/other tabs leave it `None`.
pub enum Preview {
    None,
    /// A folder is selected on the left: list what's inside it.
    Folder { path: Vec<String>, entries: Vec<Entry> },
    /// A note is selected on the left: show its body.
    Note { title: String, body: String, private: bool },
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
    /// In-flight `g`-chord, if any (see [`Pending`]).
    pending: Option<Pending>,
    pub should_quit: bool,
    pub status: String,

    pub todos: Vec<Todo>,
    pub todo_sel: usize,

    /// Current notes folder path ([] = root).
    pub cur: Vec<String>,
    pub entries: Vec<Entry>,
    pub entry_sel: usize,
    /// Right-pane preview for the current Notes selection (see [`Preview`]).
    pub preview: Preview,
    pub editor: EditorState,
    /// Persisted across keystrokes — holds multi-key Vim state (e.g. `dd`).
    edit_events: EditorEventHandler,
    /// Materialized once when entering [`Mode::Search`] (and refreshed on a
    /// live sync change) so per-keystroke filtering never re-reads every
    /// note body. Empty/ignored outside search.
    search_index: Vec<Note>,
    /// Edit timeline for the History tab (newest first). Rebuilt on entry
    /// and on a live sync change; empty/ignored off the tab.
    pub history: Vec<HistoryRow>,
    pub hist_sel: usize,

    /// Background ICS fetch thread (always spawned — fetching needs no sync
    /// server). The UI never blocks on it.
    cal: cal_sync::Handle,
    /// Cached, locally-derived (never synced) occurrences in the window,
    /// refreshed from `cal` on its dirty flag.
    pub cal_occ: Vec<Occurrence>,
    /// Per-subscription fetch status for the manager view.
    pub cal_status: Vec<CalStatus>,
    /// Calendar subscriptions (synced) — shown in the manager.
    pub cal_subs: Vec<Calendar>,
    /// Calendar tab is in the manage sub-view (list + delete) vs. agenda.
    pub cal_manage: bool,
    pub cal_sel: usize,
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

        // The calendar thread is independent of doc-sync: ICS feeds are
        // fetched directly over HTTPS, so it runs even standalone.
        let cal = cal_sync::spawn(
            store.clone(),
            std::time::Duration::from_secs(cfg.calendar_refresh_secs()),
        );

        let mut app = Self {
            store,
            sync,
            last_sync: String::new(),
            tab: Tab::Todos,
            mode: Mode::Normal,
            pending: None,
            should_quit: false,
            status: "Tab: switch · q: quit".into(),
            todos: Vec::new(),
            todo_sel: 0,
            cur: Vec::new(),
            entries: Vec::new(),
            entry_sel: 0,
            preview: Preview::None,
            editor: EditorState::new(Lines::default()),
            edit_events: EditorEventHandler::default(),
            search_index: Vec::new(),
            history: Vec::new(),
            hist_sel: 0,
            cal,
            cal_occ: Vec::new(),
            cal_status: Vec::new(),
            cal_subs: Vec::new(),
            cal_manage: false,
            cal_sel: 0,
        };
        app.refresh();
        Ok(app)
    }

    /// Brief lock on the shared store. Every call site takes it for one
    /// store operation and drops it on the same statement — the sync thread
    /// must never wait behind the UI. Poison is *recovered* rather than
    /// re-panicked: if the sync thread ever died mid-operation we still
    /// want the UI usable (the doc is an Automerge value; a half-applied
    /// transaction is impossible — commits are atomic), and one fault
    /// shouldn't cascade into a second panic here.
    fn st(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
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
        // Calendar cache is independent of doc-sync — poll it first and
        // unconditionally. Cheap atomic check; only copies on a real change.
        if self.cal.dirty.swap(false, Ordering::SeqCst) {
            self.cal_occ = self
                .cal
                .occurrences
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            self.cal_status = self
                .cal
                .statuses
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            // A name backfill writes to the doc; keep the manager list fresh.
            { let subs = self.st().calendars(); self.cal_subs = subs; }
            self.clamp();
        }

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
        // Likewise keep the timeline live while it's on screen.
        if self.tab == Tab::History {
            let h = self.st().history();
            self.history = h;
        }
        { let subs = self.st().calendars(); self.cal_subs = subs; }
        self.entries = self.build_entries();
        self.clamp();
        self.update_preview();
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

        self.children_of(&self.cur)
    }

    /// Folders-then-notes entries directly inside `folder` (one level
    /// deep). Drives both the navigable left pane (via `build_entries`)
    /// and the folder preview on the right.
    fn children_of(&self, folder: &[String]) -> Vec<Entry> {
        let notes = self.st().note_metas();
        let depth = folder.len();
        let mut dirs: Vec<String> = Vec::new();
        let mut here: Vec<&NoteMeta> = Vec::new();
        for n in &notes {
            if n.folder.len() == depth && n.folder == folder {
                here.push(n);
            } else if n.folder.len() > depth && n.folder[..depth] == *folder {
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

    /// Recompute the right-pane preview for the current Notes selection.
    /// Called after every key (post-clamp) and after a refresh, so the
    /// preview always tracks `entry_sel`/`cur`. Cheap: a folder preview is
    /// one `note_metas()` scan, a note preview one note read.
    fn update_preview(&mut self) {
        if self.tab != Tab::Notes || !matches!(self.mode, Mode::Normal) {
            self.preview = Preview::None;
            return;
        }
        self.preview = match self.selected().cloned() {
            Some(Entry::Dir(name)) => {
                let mut path = self.cur.clone();
                path.push(name);
                let entries = self.children_of(&path);
                Preview::Folder { path, entries }
            }
            Some(Entry::Note { id, .. }) => match self.st().note(&id) {
                Some(n) => Preview::Note {
                    title: if n.title.is_empty() { "(untitled)".into() } else { n.title },
                    body: n.body,
                    private: n.private,
                },
                None => Preview::None,
            },
            None => Preview::None, // the ".." row
        };
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
        if self.hist_sel >= self.history.len() {
            self.hist_sel = self.history.len().saturating_sub(1);
        }
        let cal_len = if self.cal_manage { self.cal_subs.len() } else { self.cal_occ.len() };
        if self.cal_sel >= cal_len {
            self.cal_sel = cal_len.saturating_sub(1);
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
        self.update_preview();
        Ok(())
    }

    // ---- Normal --------------------------------------------------------

    fn normal_key(&mut self, key: KeyEvent) -> Result<()> {
        // A `g`-chord in flight swallows the next key whole — before any
        // normal binding (incl. the 1-4 tab jumps and j/k) gets a look, so
        // a bookmark named `2` or `j` still works.
        if let Some(p) = self.pending.take() {
            self.pending_key(p, key);
            return Ok(());
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('g') => {
                self.pending = Some(Pending::Goto);
                self.status = "g — m: set bookmark · a-z/0-9: go to bookmark · Esc: cancel".into();
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let fwd = key.code == KeyCode::Tab;
                let next = match (self.tab, fwd) {
                    (Tab::Todos, true) => Tab::Notes,
                    (Tab::Notes, true) => Tab::Calendar,
                    (Tab::Calendar, true) => Tab::History,
                    (Tab::History, true) => Tab::Todos,
                    (Tab::Todos, false) => Tab::History,
                    (Tab::Notes, false) => Tab::Todos,
                    (Tab::Calendar, false) => Tab::Notes,
                    (Tab::History, false) => Tab::Calendar,
                };
                self.goto_tab(next);
            }
            KeyCode::Char('1') => self.goto_tab(Tab::Todos),
            KeyCode::Char('2') => self.goto_tab(Tab::Notes),
            KeyCode::Char('3') => self.goto_tab(Tab::Calendar),
            KeyCode::Char('4') => self.goto_tab(Tab::History),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            _ => match self.tab {
                Tab::Todos => self.todos_key(key)?,
                Tab::Notes => self.notes_key(key)?,
                Tab::Calendar => self.cal_key(key)?,
                Tab::History => self.hist_key(key)?,
            },
        }
        Ok(())
    }

    fn goto_tab(&mut self, tab: Tab) {
        self.tab = tab;
        if self.tab == Tab::History {
            self.enter_history();
        } else if self.tab == Tab::Calendar {
            self.enter_calendar();
        }
    }

    /// Second/third keystroke of a `g`-chord. `Esc` (or any non-bookmark
    /// key) cancels; bookmark keys are the alphanumerics minus `m`, which
    /// is reserved as the `gm` set-prefix.
    fn pending_key(&mut self, p: Pending, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.status = "Cancelled".into();
            return;
        }
        match p {
            Pending::Goto => match key.code {
                KeyCode::Char('m') => {
                    self.pending = Some(Pending::SetMark);
                    self.status =
                        "Set bookmark — press a-z/0-9 (on the note to bookmark) · Esc: cancel"
                            .into();
                }
                KeyCode::Char(c) if c.is_alphanumeric() => self.jump_mark(c),
                _ => self.status = "Not a bookmark key".into(),
            },
            Pending::SetMark => match key.code {
                KeyCode::Char(c) if c.is_alphanumeric() => self.set_mark(c),
                _ => self.status = "Not a bookmark key".into(),
            },
        }
    }

    /// `gm{c}`: point bookmark `c` at the note under the cursor. Only the
    /// Notes tab has a "current note"; anywhere else there's nothing to
    /// bookmark.
    fn set_mark(&mut self, c: char) {
        let Some(Entry::Note { id, title, .. }) =
            (self.tab == Tab::Notes).then(|| self.selected().cloned()).flatten()
        else {
            self.status = "Select a note (Notes tab) before setting a bookmark".into();
            return;
        };
        let r = self.st().set_mark(c, &id);
        match r {
            Ok(()) => {
                self.persist();
                self.status = format!("Bookmark '{c}' → {title}");
            }
            Err(e) => self.status = format!("Set bookmark failed: {e}"),
        }
    }

    /// `g{c}`: open the bookmarked note in the editor from anywhere. A
    /// stale bookmark (note since deleted) reports rather than silently
    /// doing nothing.
    fn jump_mark(&mut self, c: char) {
        let Some(id) = self.st().mark(c) else {
            self.status = format!("No bookmark '{c}'");
            return;
        };
        if !self.open_note_by_id(&id) {
            self.status = format!("Bookmark '{c}' points at a deleted note");
        }
    }

    /// Switch to the Notes tab, descend to the note's folder, select it,
    /// and open it in the editor. Returns `false` (no-op) if the id no
    /// longer resolves. Shared by bookmark-jump and any future jump-to.
    fn open_note_by_id(&mut self, id: &str) -> bool {
        let Some(note) = self.st().note(id) else {
            return false;
        };
        self.tab = Tab::Notes;
        self.cur = note.folder.clone();
        self.entries = self.build_entries();
        // Land the list cursor on the note too, so closing the editor
        // returns here in context rather than to a stale selection. The
        // leading ".." row (present unless the list is flat) shifts the
        // index by one — mirror `selected()`'s convention.
        if let Some(i) = self.entries.iter().position(
            |e| matches!(e, Entry::Note { id: nid, .. } if nid == id),
        ) {
            self.entry_sel = if self.flat_list() { i } else { i + 1 };
        }
        self.editor = make_editor(&note.body);
        self.mode = Mode::NoteEdit { id: id.to_string(), title: note.title };
        self.status = "Editing (NORMAL) — i insert · q save & close".into();
        true
    }

    fn move_sel(&mut self, delta: isize) {
        let rows = self.note_rows();
        let (sel, len) = match self.tab {
            Tab::Todos => (&mut self.todo_sel, self.todos.len()),
            Tab::Notes => (&mut self.entry_sel, rows),
            Tab::Calendar => (
                &mut self.cal_sel,
                if self.cal_manage { self.cal_subs.len() } else { self.cal_occ.len() },
            ),
            Tab::History => (&mut self.hist_sel, self.history.len()),
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

    // ---- History -------------------------------------------------------

    fn enter_history(&mut self) {
        let h = self.st().history();
        self.history = h;
        self.hist_sel = 0;
        self.status =
            "History — ↑↓ select · p preview · r restore (whole corpus) · c name a snapshot"
                .into();
    }

    fn hist_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('c') => {
                self.mode = Mode::Input {
                    prompt: Prompt::NewCheckpoint,
                    buffer: String::new(),
                };
                self.status = "Snapshot reason — Enter: create · Esc: cancel".into();
            }
            KeyCode::Char('p') | KeyCode::Enter => {
                if let Some(row) = self.history.get(self.hist_sel).cloned() {
                    // Bind first so the store guard is dropped before we
                    // touch `self.status` (st() borrows self).
                    let res = self.st().preview_restore_to(&row.hash);
                    self.status = match res {
                        Ok(r) => format!(
                            "Preview: notes +{}/~{}/-{} · todos +{}/~{}/-{} — press r to \
                             restore the WHOLE corpus to this point",
                            r.notes_added, r.notes_updated, r.notes_deleted,
                            r.todos_added, r.todos_updated, r.todos_deleted,
                        ),
                        Err(e) => format!("Preview failed: {e}"),
                    };
                }
            }
            KeyCode::Char('r') => {
                if let Some(row) = self.history.get(self.hist_sel).cloned() {
                    let res = self.st().restore_to(&row.hash);
                    let msg = match res {
                        Ok(r) => {
                            self.persist();
                            format!(
                                "Restored to {} — notes +{}/~{}/-{} · todos +{}/~{}/-{}",
                                &row.hash[..8.min(row.hash.len())],
                                r.notes_added, r.notes_updated, r.notes_deleted,
                                r.todos_added, r.todos_updated, r.todos_deleted,
                            )
                        }
                        Err(e) => format!("Restore failed: {e}"),
                    };
                    self.refresh();
                    self.status = msg;
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ---- Calendar ------------------------------------------------------

    fn enter_calendar(&mut self) {
        { let subs = self.st().calendars(); self.cal_subs = subs; }
        self.cal_sel = 0;
        self.status = if self.cal_subs.is_empty() {
            "Calendar — a: add a calendar (paste an ICS URL) · no calendars yet".into()
        } else {
            "Calendar — a add · r refresh · m manage/delete".into()
        };
    }

    fn cal_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('a') => {
                self.mode = Mode::Input { prompt: Prompt::AddCalendar, buffer: String::new() };
                self.status = "Paste an ICS URL — Enter: subscribe · Esc: cancel".into();
            }
            KeyCode::Char('r') => {
                self.cal.refresh_now.store(true, Ordering::SeqCst);
                self.status = "Refreshing calendars…".into();
            }
            KeyCode::Char('m') => {
                self.cal_manage = !self.cal_manage;
                self.cal_sel = 0;
                self.status = if self.cal_manage {
                    "Manage calendars — ↑↓ select · d delete · m back to agenda".into()
                } else {
                    "Calendar — a add · r refresh · m manage/delete".into()
                };
            }
            KeyCode::Char('d') if self.cal_manage => {
                if let Some(c) = self.cal_subs.get(self.cal_sel).cloned() {
                    self.st().remove_calendar(&c.id)?;
                    self.persist();
                    { let subs = self.st().calendars(); self.cal_subs = subs; }
                    // Drop its events immediately; a refetch confirms.
                    self.cal_occ.retain(|o| o.calendar != c.name);
                    self.cal.refresh_now.store(true, Ordering::SeqCst);
                    self.clamp();
                    let label = if c.name.is_empty() { c.url } else { c.name };
                    self.status = format!("Removed calendar '{label}'");
                }
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
                    Prompt::NewCheckpoint => {
                        let msg = if text.is_empty() {
                            "Empty reason — snapshot cancelled".to_string()
                        } else {
                            let res = self.st().create_checkpoint(&text);
                            match res {
                                Ok(_) => {
                                    self.persist();
                                    format!("Snapshot created: {text}")
                                }
                                Err(e) => format!("Snapshot failed: {e}"),
                            }
                        };
                        self.mode = Mode::Normal;
                        self.refresh();
                        self.status = msg;
                    }
                    Prompt::AddCalendar => {
                        let url = text;
                        let msg = if url.is_empty() {
                            "Empty URL — cancelled".to_string()
                        } else if !(url.starts_with("http://")
                            || url.starts_with("https://")
                            || url.starts_with("webcal://"))
                        {
                            "Not a URL — expected http(s):// or webcal://".to_string()
                        } else {
                            // webcal:// is just ICS-over-HTTPS with a custom
                            // scheme; normalise so the fetcher can GET it.
                            let url = url
                                .strip_prefix("webcal://")
                                .map(|r| format!("https://{r}"))
                                .unwrap_or(url);
                            let res = self.st().add_calendar(&url, "");
                            match res {
                                Ok(_) => {
                                    self.persist();
                                    { let subs = self.st().calendars(); self.cal_subs = subs; }
                                    self.cal.refresh_now.store(true, Ordering::SeqCst);
                                    "Calendar added — fetching…".to_string()
                                }
                                Err(e) => format!("Add failed: {e}"),
                            }
                        };
                        self.mode = Mode::Normal;
                        self.status = msg;
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
