# ccal — agent guide

A terminal (ratatui/crossterm) **todo list + markdown notes** app in Rust.
Two distinct things:

- **Todos** — an ordered set of strings, reorderable, each carrying a set
  of free-text **tags**; the list view can be filtered to one tag.
- **Notes** — named markdown documents, browsed through a **folder tree**.
  Folders are not an entity: they are *derived* from each note's `folder`
  path array.

Notes were originally seeded by a one-shot import from the Bear app; after
that, ccal is the source of truth and Bear is irrelevant.

## Workspace layout

Single package, one library + two binaries:

```
src/lib.rs            crate `ccal`  ── pub mod models; store; calendar;
src/models.rs         pure domain types, no Automerge / no I/O
src/store.rs          THE ONLY place that knows Automerge exists
src/calendar.rs       pure ICS parse + recurrence expand, no I/O / no net
src/main.rs           `ccal` TUI binary entry (event loop, terminal setup)
src/app.rs            TUI state machine + key handling   (binary-private)
src/ui.rs             ratatui rendering                  (binary-private)
src/sync_client.rs    background doc-sync thread         (binary-private)
src/cal_sync.rs       background ICS fetch thread        (binary-private)
src/server_mcp.rs     optional embedded MCP server  (ccal-server-private,
                      pulled in via #[path] — never compiled into the lib)
src/bin/import-bear.rs `import-bear` binary — standalone one-shot importer
src/bin/ccal-server.rs  `ccal-server` binary — Automerge sync peer (+ MCP)
```

Hard rule: `import-bear`, `ccal-server` and the TUI share **only** the pure
lib (`ccal::store` / `ccal::models` / `ccal::calendar`). They must never
touch `app`/`ui`; nothing outside `store.rs` may use the `automerge` crate.
`calendar.rs` is pure (no I/O, no net) — the network fetch lives in the
TUI-private `cal_sync.rs`, exactly as transport stays out of the lib. All async/transport
(tokio, axum) lives in `ccal-server` only — the lib stays tokio-free.

## Data model

Persisted as ONE Automerge document: `<data_dir>/ccal.automerge`
(`~/Library/Application Support/ccal/` on macOS). Saved atomically
(temp file + rename).

ROOT map:
- `schema`: Int
- `notes`: Map  id → `{ title:Str, folder:List<Str>, body:Text,
  created:Int(ms), modified:Int(ms), private:Bool? (absent = false) }`
- `todos`: Map  id → `{ text:Str, order:F64, created:Int(ms),
  tags:List<Str>? (absent = none) }` — `tags` is a per-todo field
  (like a note's `folder`/`private`), so it rides sync with no genesis
  or ROOT-key change; reconciled on restore like any field.
- `checkpoints`: Map  id → `{ reason:Str, created:Int(ms),
  heads:List<Str> }` — **lazily created**, NOT in genesis (see below)

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
- **`body` is an Automerge `List<Str>`** — one element per line. Concurrent
  edits to *different* lines merge; the *same* line resolves last-writer-wins.
  (Was a per-character `Text` CRDT through schema 1; changed to lines in
  schema 2 because char-level cost O(every character in the corpus) at load —
  ~800 K ops / ~1.2 s in WASM — vs O(lines), ~30× fewer ops. Edits splice
  only the changed line range; `Store::migrate_v1_in_place` re-genesises an
  old replica, dropping the old `Text` op history.)
- **Folders** are derived: a note at `["a","b"]` appears under `a/b`.
  Deleting a folder = deleting the notes in it; there is no folder object.
- **Todo order**: fractional `order: f64`; reorder swaps two todos' keys
  (`Store::swap_todo_order`); list sorted by `(order, id)`.
- **Checkpoints** (for the MCP "let the LLM safely reorganise" story).
  Automerge never prunes history, so a checkpoint copies nothing — it's
  just `{reason, created, heads}` where `heads` are the hex
  `get_heads()` at creation. **Restore is a forward change, not a
  rewind** (a CRDT can't subtract): `restore_checkpoint` does
  `fork_at(heads)`, diffs that snapshot against the live corpus, and
  applies the minimal upserts/deletes in ONE transaction (note bodies via
  the shared `text_splice` so concurrent char edits still merge). It is
  therefore *whole-corpus* — it also reverts edits made after the
  checkpoint, even on other devices (fine under the single-operator model;
  the returned `RestoreReport` shows the blast radius). Because it's just
  another transaction, it syncs + persists through the existing path with
  zero special-casing — proven by `checkpoint_restores_whole_corpus_and_syncs`.
  - **Why `checkpoints` isn't in `genesis_doc()`:** adding a ROOT key to
    genesis changes the genesis bytes and desyncs every existing replica
    (same hazard as the pre-genesis Bear doc). Instead the map is created
    **lazily by the first checkpoint write**, and `open_at` only *resolves*
    it (never creates). That's safe **only because there is exactly one
    writer** — the single always-on `ccal-server` (the only place the MCP
    server runs) — so the "two peers each seed their own ROOT map" genesis
    hazard can't occur. `receive_sync_message` re-resolves it, same as
    `notes`/`todos`. A second independent checkpoint writer would break
    this assumption — revisit if that ever happens.
- **Private notes** (`private` bool, absent = false). User-only "hide this
  from the LLM" — set in the TUI (`p`), **never** via MCP (no tool exists,
  so a model can't un-hide a note to read it). Enforced **only at the MCP
  boundary**: `note_json` always swaps a private body for a redaction
  string and `update_note_body` refuses; the TUI and sync keep the real
  content (this is LLM-scoping, NOT encryption at rest — consistent with
  the plaintext trust model). The LLM may still rename/move/delete a
  private note (user's call: delete is recoverable via checkpoints). It is
  a plain synced field (rides sync like title/folder, no genesis impact).
  - **Non-retroactive — deliberate (user decision).** `private` reconciles
    on restore like any field; restoring to a checkpoint where the note
    was public legitimately returns that older, then-unprotected body with
    `private=false`. So privacy protects from the moment it's set, NOT
    snapshots/history that predate it. The TUI toast on toggling private
    says as much; mark notes private *before* adding secrets. Do not
    "fix" this by special-casing private notes in restore — it was
    considered and explicitly rejected (would make restore lossy/uncanny;
    the accepted limitation is simpler and was the user's choice). Covered
    by `private_flag_syncs_and_is_non_retroactive_on_restore`.

## Critical performance constraint

Automerge op-processing is **~1000× slower in a debug build**. This was acute
when `body` was a per-character `Text` (one op per character → ~800 K ops for
644 notes): import never completed under `cargo build`, and a cold WASM load
took ~1.2 s. Schema 2's line-based `body: List<Str>` cut that to ~30 K ops
(~96 ms WASM load), but debug Automerge is still slow enough to matter at
scale. Therefore:

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

Toolchain: needs **rustc ≥ 1.89** (Automerge 0.9). Dev machine is on
Homebrew rust 1.95.

**Automerge `MissingOps` panic — do NOT reintroduce `get_changes`/`fork_at`.**
`Automerge::get_changes(&[])` and `fork_at(heads)` reconstruct changes from
the op-set and internally `unwrap()` an `Err(MissingOps)` when the op range
has holes (normal once history has merged via sync). It's an upstream panic
present *unchanged in 0.7.4 → 0.9.0* (not a version bug; upgrading doesn't
fix it). Therefore: `Store::history()` uses `get_changes_meta(&[])`
(change-graph metadata only, no op reconstruction) and `plan_restore_to`
reads past state with the clock-based `*_at` ReadDoc API instead of
forking. All free readers take one `At` clock param (`At::Now` →
`d.get`/`keys`/`text`; `At::Heads(h)` → `d.get_at`/`keys_at`/`text_at`):
one `note_view`/`all_notes`/`all_todos`/`folder` definition serves both
live reads and restore, so a new note field can't be added to one path and
forgotten on the other. `plan_restore_to` also validates every target head
via `get_change_meta_by_hash` first: unknown heads would make every `*_at`
read empty and a restore would then wipe the corpus. Keep it this way.

UI stack: **ratatui 0.30** + **edtui 0.11** (Vim editor, syntect markdown
highlighting). crossterm is **not a direct dependency** — always import it
via `ratatui::crossterm` so its version stays aligned with both ratatui and
edtui. tui-textarea was removed.

## TUI keys

Global: `Tab`/`BackTab` cycle Todos→Notes→History · `q` quit · `j/k`/arrows
move.
Todos: `a` add · `e`/`Enter` edit · `d` delete · `J`/`K` reorder ·
`Space` multi-select · `t` tag the selected/marked todos · `f` filter the
list to a tag. `t`/`f` open a prompt with **Tab tag-autocomplete** (cycles
existing tags by typed prefix). A filter is shown in the list title and
**auto-applied to new todos** (`a`) so they don't vanish from the view;
empty `f` Enter clears it. `Store::tag_todos` (bulk, one tx, idempotent) /
`set_todo_tags` are the only tag writers — no MCP tag tool (TUI-only, like
note privacy); MCP `list_todos` does surface `tags` read-only.
Notes: `Enter`/`→` open or descend · `←`/`h` up · `n` new · `R` rename ·
`m` move note · `p` toggle private (hide body from the LLM) · `/` search ·
`d` delete note · `r` reload store from disk (pick up an external
`import-bear` run).
- `/` (Notes view) enters **search mode** (`Mode::Search{query}`): the
  folder tree collapses to a flat, live-filtered list of matches across
  the *whole* corpus (title/folder/body, full content — the user owns it).
  ↑↓ select · Enter opens · Backspace edits · Esc cancels. The corpus is
  materialized once on `/` into `App::search_index` (and refreshed on a
  live sync change) so per-keystroke filtering never re-reads bodies.
  `flat_list()` (root **or** searching) drives the no-".."-row logic.
History tab: the edit timeline, newest first (`Store::history()` →
`Vec<HistoryRow>`: every Automerge change as hash/ts/ops/actor, plus the
checkpoint `reason` inline when a change is a named snapshot's head). Keys:
`↑↓` select · `p`/`Enter` preview the blast radius · `r` restore (whole
corpus, **time-travel to any change**, not just checkpoints) · `c` create a
named snapshot (`Prompt::NewCheckpoint` → `create_checkpoint`).
- Restore reuses the checkpoint engine: `plan_restore_to(&heads)` +
  `apply_restore`; `restore_to(hash)`/`preview_restore_to(hash)` are the
  arbitrary-point entry points, `restore_checkpoint` just resolves a
  checkpoint's heads first. So time-travel is *one ordinary forward
  change* and syncs/persists with zero special-casing (proven by
  `history_lists_changes_and_time_travels`).
- **Commits are timestamped.** All interactive commits go through the
  private `commit(tx)` helper (`CommitOptions::with_time(now_secs())`) so
  the timeline has a real clock; the timestamp is advisory (not used in
  merge) so convergence is unaffected. **`genesis_doc` is the lone
  exception** — it must stay `with_time(0)` to keep the byte-identical
  ancestor; never route it through `commit()`. Pre-existing changes (and
  genesis) have ts 0 → shown as "—".

- **Folders are created implicitly** — there is no "make folder" command
  (none can exist: folders are derived, an empty one has nothing to derive
  from). You make a folder by *filing a note into it*, two ways:
  - `n` (new): the typed name is a **path**. Either separator (`/` or `\`)
    splits it; the **last** segment is the note title, anything before it is
    a folder path **relative to the folder you're in**. Blank/whitespace
    segments are dropped. `meeting` → note at the current level (unchanged);
    `work/ideas/standup` → note "standup" in `work/ideas`, creating both.
  - `m` (move): prompt for a slash/back-slash path (prefilled with the
    note's current path, blank = root); the note relocates there, creating
    folders as needed.
- `R` (rename): on a note → retitle; on a folder → recursive prefix rewrite
  over the **whole subtree** (every note whose path starts with it). The new
  value must be a single path component (no separator).
- Both `n` and `m` share `App::parse_path` (binary-private, in `app.rs`):
  splits on `['/', '\\']`, trims, drops empties.
- Sync: `set_note_title`/`set_note_folder`/`rename_folder` are plain
  field/list writes — they ride the existing Automerge sync path with no
  schema or genesis change (folders are still purely derived). Concurrent
  *rename of the same folder to different names* on two replicas is
  last-writer-wins per note (notes may split across both names); body edits
  to different lines still merge (line-level `List<Str>`).

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
body still merges in the doc line-by-line; reconciled on next open). Config is env:
`CCAL_SYNC_URL` (e.g. `ws://host:8787/sync/ccal`) + `CCAL_SYNC_TOKEN`; absent
either ⇒ standalone, same code path, no thread.

- Tests: `cargo test` — lib convergence unit test, `tests/sync_e2e.rs`
  (tokio-tungstenite ↔ real server), `tests/sync_client_transport.rs`
  (the *blocking* client transport ↔ real server: bearer handshake,
  non-blocking pump, convergence). Each test uses its own port.

## MCP server (optional, embedded in `ccal-server`)

An MCP server for LLM coding assistants to organise/summarise the corpus —
**opt-in**, off unless `CCAL_MCP` / `[server] mcp` is set. Lives in
`src/server_mcp.rs`, `ccal-server`-private (pulled in with
`#[path] mod mcp;`, exactly like the TUI's binary-private modules) so the
lib stays tokio-free and Automerge stays sealed in `store.rs` — the same
hard rule as the rest of the codebase.

- **Why embedded, not a standalone stdio peer:** `ccal-server` already
  holds the shared `Doc { store, changed, dirty }` and rebroadcasts to
  every connected peer. An MCP tool just does what `serve_peer` does after
  a received change — mutate `doc.store`, then `dirty.notify_one()` +
  `changed.send(())`. So an assistant's edits reach every open TUI **live**
  through the existing, proven sync path with zero new sync code. (A
  client-side stdio binary was the alternative; rejected — third replica,
  its own reconnect logic, still needs the server running anyway.)
- Transport: rmcp (`modelcontextprotocol/rust-sdk`) **streamable-HTTP**, a
  generic `tower` Service nested at `/mcp` on the *same* axum listener — no
  axum bump, WS sync path untouched. A daemon can't be stdio.
- Auth/trust: identical to sync — a `Bearer <token>` middleware (same token
  as the WS path) in front of `/mcp`; `disable_allowed_hosts()` because the
  gate is the token + Tailscale/TLS, and the client is a CLI assistant not
  a browser. Full read+write surface is *why* the whole thing is opt-in.
- Tools (18): `list_notes` (body-free, optional folder-prefix scope),
  `search_notes` (title/folder/body, privacy-aware), `get_note`,
  `create_note`, `set_note_title`, `update_note_body`, `move_note`,
  `rename_folder`, `delete_note`, `list_todos`, `add_todo`,
  `set_todo_text`, `swap_todos`, `delete_todo`, plus checkpoints:
  `create_checkpoint`, `list_checkpoints`, `preview_restore`,
  `restore_checkpoint` — all through the existing `Store` facade; each
  mutation (restore included) rides the live-sync notify path above.
- **Privacy boundary lives here.** `note_json` redacts a private note's
  body for *every* tool that returns a note (so no path can leak by
  forgetting); `update_note_body` refuses on a private note (check +
  mutate under one lock). `list_notes`/`get_note` expose the `private`
  flag so the model knows. No tool sets privacy — that's TUI-only by
  design. Restore/preview never expose bodies (only counts).
  `search_notes` calls `Store::search_notes(q, false)` so a private
  note can only match its title/folder, never its body — you can't probe
  hidden contents by querying for a phrase and seeing it match. The
  `get_info` instructions tell the model: private bodies are unknown,
  don't try to work around it; rename/move/delete still allowed.
- The `get_info` instructions impose **checkpoint discipline**: before a
  batch of edits the LLM must `create_checkpoint` with a reason, again
  after, and `preview_restore` before any `restore_checkpoint`. This is
  prompt-only (no enforcement) — the user chose explicit-LLM over an
  auto-checkpoint-per-session; revisit if models skip it in practice.
- Config: `CCAL_MCP` (1/true/yes/on) enables it; `CCAL_MCP_DOC` (default
  `ccal`) is the docid it edits — must match the clients' sync docid (the
  last path segment of their `url`) or the assistant edits a different
  replica. Connect: `claude mcp add --transport http ccal
  http://host:8787/mcp --header "Authorization: Bearer <token>"`.
- Smoke-tested manually (initialize → tools/list → CRUD → checkpoint /
  preview / restore round-trip → search → 401 on bad token). Checkpoint/
  restore *logic* + sync-cleanliness is unit-tested
  (`checkpoint_restores_whole_corpus_and_syncs`); privacy + non-retroactive
  restore by `private_flag_syncs_and_is_non_retroactive_on_restore`;
  search + its privacy boundary by `search_respects_privacy_boundary`
  (the redacted/blocked paths can't be hit live since marking private is
  TUI-only); live MCP→peer propagation is the *same* `dirty`/`changed`
  calls `sync_e2e` already proves converge — not separately automated yet.

**Still open:** wss:// in the client; `Store` reload during live sync forces
a full peer resync (acceptable for the import-bear case); concurrent remote
edit to the note currently open in the editor isn't live-reconciled in the
buffer (merges in the doc, shows on reopen). iOS client later. See the agent
memory decision log under `.claude/projects/.../memory/`.
