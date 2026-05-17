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
src/sync_client.rs    background sync thread             (binary-private)
src/bin/import-bear.rs `import-bear` binary — standalone one-shot importer
src/bin/ccal-server.rs  `ccal-server` binary — Automerge sync peer
```

Hard rule: `import-bear`, `ccal-server` and the TUI share **only**
`ccal::store` / `ccal::models`. They must never touch `app`/`ui`; nothing
outside `store.rs` may use the `automerge` crate. All async/transport
(tokio, axum) lives in `ccal-server` only — the lib stays tokio-free.

## Data model

Persisted as ONE Automerge document: `<data_dir>/ccal.automerge`
(`~/Library/Application Support/ccal/` on macOS). Saved atomically
(temp file + rename).

ROOT map:
- `schema`: Int
- `notes`: Map  id → `{ title:Str, folder:List<Str>, body:Text,
  created:Int(ms), modified:Int(ms) }`
- `todos`: Map  id → `{ text:Str, order:F64, created:Int(ms) }`

- **Genesis.** A fresh replica starts from `genesis_doc()` — the canonical
  empty doc built with a FIXED actor + FIXED commit time, so the genesis
  change is byte-identical everywhere and all replicas share one ancestor
  (`notes`/`todos` resolve to the same ObjId). Without this, two blank
  replicas make conflicting root maps that never converge. Immediately after,
  each open sets a random actor for its own edits (else replicas collide as
  the genesis actor → "duplicate seq"). **Migration caveat:** any
  pre-genesis on-disk doc (the original Bear-import `ccal.automerge`) will
  NOT converge with genesis replicas — re-run `import-bear` into a genesis
  doc before relying on sync.
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
CCAL_SYNC_TOKEN=… cargo run --release --bin ccal-server   # sync peer
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

## Sync (`ccal-server`)

Multi-device sync via **Automerge's own sync protocol** — DECIDED. The
server is a tiny always-on **peer**: it loads the same Automerge doc, runs
the protocol per connection, merges server-side, rebroadcasts, and is a free
plaintext backup. No DB engine, no schema, no migrations.

- `store.rs` exposes a thin facade so Automerge stays encapsulated:
  `Store::open_at(path)`, `generate_sync_message`, `receive_sync_message`;
  `SyncState` is re-exported from the crate root. Remote changes land via
  this low-level path, **never** `AutoCommit`.
- Wire protocol (language-neutral, for future `automerge-swift` iOS client):
  `ws://host/sync/{docid}`, `Authorization: Bearer <token>` checked at the
  handshake, every binary frame = raw `automerge::sync::Message` bytes.
- Trust model: operator owns the box → server-as-peer, plaintext at rest,
  no E2EE. Deployment puts TLS/Tailscale in front; the token check is in the
  server regardless. Config env: `CCAL_SYNC_TOKEN` (required),
  `CCAL_SYNC_ADDR` (default `127.0.0.1:8787`), `CCAL_SYNC_DATA`.
**TUI client** (`src/sync_client.rs`, binary-private): one OS thread,
blocking `tungstenite`, **ws:// only** (run inside Tailscale; wss:// is a
later feature, not a rewrite). The doc is shared with the UI thread via
`Arc<Mutex<Store>>`; the lock is held only for individual generate/receive/
save calls, **never across network IO or a redraw** (`App::st()`). After the
handshake the socket goes non-blocking so the pump also flushes local edits
promptly. The thread sets a `dirty` flag + a status string; `App::tick()`
(called once per UI loop, before draw) folds remote changes in via
`refresh()` — it deliberately does **not** touch an open editor buffer (the
Text CRDT still merges in the doc; reconciled on next open). Config is env:
`CCAL_SYNC_URL` (e.g. `ws://host:8787/sync/ccal`) + `CCAL_SYNC_TOKEN`; absent
either ⇒ standalone, same code path, no thread.

- Tests: `cargo test` — lib convergence unit test, `tests/sync_e2e.rs`
  (tokio-tungstenite ↔ real server), `tests/sync_client_transport.rs`
  (the *blocking* client transport ↔ real server: bearer handshake,
  non-blocking pump, convergence). Each test uses its own port.

**Still open:** wss:// in the client; `Store` reload during live sync forces
a full peer resync (acceptable for the import-bear case); concurrent remote
edit to the note currently open in the editor isn't live-reconciled in the
buffer (merges in the doc, shows on reopen). iOS client later. See the agent
memory decision log under `.claude/projects/.../memory/`.
