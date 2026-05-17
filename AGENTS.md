# ccal — agent guide

A terminal (ratatui/crossterm) **todo list + markdown notes** app in Rust.
Two distinct things:

- **Todos** — an ordered set of strings, reorderable.
- **Notes** — named markdown documents, browsed through a **folder tree**.
  Folders are not an entity: they are *derived* from each note's `folder`
  path array.

Notes were originally seeded by a one-shot import from the Bear app; after
that, ccal is the source of truth and Bear is irrelevant.

## Workspace layout

Single package, one library + two binaries:

```
src/lib.rs            crate `ccal`  ── pub mod models; pub mod store;
src/models.rs         pure domain types, no Automerge / no I/O
src/store.rs          THE ONLY place that knows Automerge exists
src/main.rs           `ccal` TUI binary entry (event loop, terminal setup)
src/app.rs            TUI state machine + key handling   (binary-private)
src/ui.rs             ratatui rendering                  (binary-private)
src/bin/import-bear.rs `import-bear` binary — standalone one-shot importer
```

Hard rule: `import-bear` and the TUI share **only** `ccal::store` /
`ccal::models`. The importer must never touch `app`/`ui`; nothing outside
`store.rs` may use the `automerge` crate.

## Data model

Persisted as ONE Automerge document: `<data_dir>/ccal.automerge`
(`~/Library/Application Support/ccal/` on macOS). Saved atomically
(temp file + rename).

ROOT map:
- `schema`: Int
- `notes`: Map  id → `{ title:Str, folder:List<Str>, body:Text,
  created:Int(ms), modified:Int(ms) }`
- `todos`: Map  id → `{ text:Str, order:F64, created:Int(ms) }`

- **Identity** is app-owned UUID v4 (`models::new_id`). External keys
  (Bear's) are deliberately never reused.
- **`body` is an Automerge `Text` object** (per-character CRDT) so
  concurrent edits to the same note merge at character granularity.
- **Folders** are derived: a note at `["a","b"]` appears under `a/b`.
  Deleting a folder = deleting the notes in it; there is no folder object.
- **Todo order**: fractional `order: f64`; reorder swaps two todos' keys
  (`Store::swap_todo_order`); list sorted by `(order, id)`.

## Critical performance constraint

Automerge's per-char `Text` CRDT is **~1000× slower in a debug build**. A
644-note import never completes under `cargo build`; under
`--release` it takes ~0.4 s. Therefore:

- **Always run everything — the TUI included — with `--release`.** A debug
  build is unusably slow (adding a note / opening the list takes seconds).
- The folder tree uses `Store::note_metas()` (no body materialization).
  Only materialize a body (`Store::note`) when actually opening a note —
  never scan all bodies just to list.
- `store.rs` uses the low-level `automerge::Automerge` API with **explicit
  transactions**, NOT `AutoCommit` (AutoCommit per-op bookkeeping is
  pathological at scale). Interactive edits = one small transaction each;
  bulk import = a single transaction over all notes
  (`Store::import_notes`). Keep it that way.
- Interactive `set_note_body` computes a minimal prefix/suffix diff and
  splices only the changed region — do not replace the whole Text.

## Build & run

```
cargo run --release                 # the TUI
cargo run --release --bin import-bear   # one-shot Bear → store import
cargo build --release               # everything
```

`import-bear` reads Bear's SQLite **read-only** (snapshots the db + any
WAL/SHM to a temp dir, queries via the system `sqlite3 -json`). Skips
trashed/archived/encrypted notes. Files a note under its most specific tag;
nested tags (`a/b`) → nested folders; untagged → `Untagged`. It is additive
— re-running appends duplicates (one-shot by design).

Toolchain: needs **rustc ≥ 1.89** (Automerge 0.7). Dev machine is on
Homebrew rust 1.95.

UI stack: **ratatui 0.30** + **edtui 0.11** (Vim editor, syntect markdown
highlighting). crossterm is **not a direct dependency** — always import it
via `ratatui::crossterm` so its version stays aligned with both ratatui and
edtui. tui-textarea was removed.

## TUI keys

Global: `Tab` switch view · `q` quit · `j/k`/arrows move.
Todos: `a` add · `e`/`Enter` edit · `d` delete · `J`/`K` reorder.
Notes: `Enter`/`→` open or descend · `←`/`h` up · `n` new (in current
folder) · `d` delete note · `r` reload store from disk (pick up an external
`import-bear` run).

**Note editor is modal (Vim, via edtui).** This is deliberate: app commands
only fire in the editor's **Normal** mode, so typing in Insert/Visual/Search
never triggers an app action — there is no key-conflict layer to maintain.
- Open existing note → starts in **Normal**; `i` to insert.
- New note → starts in **Insert** (type immediately).
- `Esc` in Insert → edtui Normal (NOT intercepted by the app).
- In **Normal**: `q` or `Esc` → save & return to list.
- `Ctrl+S` (any mode) → save, stay in editor.
Routing lives in `App::editor_key`; `App::edit_events`
(`EditorEventHandler`) is persisted across keystrokes because it holds
multi-key Vim state (`dd`, counts, …). Do not recreate it per event.

## Direction (not yet built)

Multi-device sync, leaning **Automerge sync** (standalone vs. connected is
one code path; tiny relay peer; native iOS via Swift bindings). CouchDB was
considered and set aside. See the agent memory under
`.claude/projects/.../memory/` for the live decision log.
