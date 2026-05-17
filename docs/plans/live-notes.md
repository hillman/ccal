# Live notes plan — file replicas & server-fetched URL notes

Status: planned. Decisions made 2026-05-17 (URL→text pipeline verified with a
throwaway prototype against live pages — see "Pipeline, proven").

## Goal & scope

Two ways a note's content can be *driven from outside the doc*, from
`docs/ideas.md`:

1. **Live file replica** — a note backed by a local markdown file. Open it
   and you see/edit the file; close it and the file holds the truth.
2. **Live URL note** — a note whose body is a readable rendering of a fetched
   web resource (news, a webhook's markdown, an article), refreshed in the
   background by `ccal-server`.

The two halves deliberately have **opposite sync models**, and that is the
whole design, not an accident:

| | Pointer (path / url) | Body |
|---|---|---|
| **File note** | syncs | **not synced** — the filesystem *is* the store |
| **URL note** | syncs | **syncs** — `ccal-server` is the sole writer |

## Decisions

- **File note: body is not in the Automerge doc.** Only a small `source`
  pointer (`{kind:"file", path}`) syncs, exactly like a calendar
  subscription (`ROOT["cal/<id>"]`). The file on disk *is* the document.
  This deletes the hard problem rather than solving it: there is no CRDT
  replica of the content, so "two-way sync" is just an ordinary text editor
  (read on open, write on save) with **no** git-pull-vs-local-edit CRDT
  merge to reason about. The only conflict left is the everyday one — the
  file changed underneath an open buffer — handled with an mtime check and a
  warning, not CRDT machinery. A device that does not have the file shows a
  placeholder, never stale synced content. This is precisely the
  `ideas.md` line: *"machines that ... don't have the relevant file ...
  just use what's there."* It also matches the established `cal_sync`
  philosophy (only the small user-authored thing syncs; derived content is
  local).

- **URL note: `ccal-server` is the single writer.** A per-doc background
  task on the server fetches on an interval and writes the body. Server-only
  because: it is always-on, it is a single well-known writer (no
  multi-device race to splice one body — the exact hazard the calendar
  design avoids), and it can write while no TUI is open. It rides the
  existing change-broadcast, so a refreshed note appears live in every TUI
  and over MCP (same mechanism the embedded MCP server already uses). If no
  server runs, URL notes simply do not refresh — consistent with the file
  case ("just use what's there").

- **Body is spliced only when it actually changed.** A diff-and-skip guard:
  if the freshly rendered text equals the current body, the transaction is
  not opened at all. Without this, a 15-minute refresh loop bloats document
  history forever — the precise failure mode the `cal_sync` module comment
  and `automerge-store-design` memory warn about. Fetch status / last-ok /
  errors are **never** written to the doc; they live in an in-memory handle
  (a `LiveStatus`, shaped like `cal_sync::CalStatus`) surfaced in a manager
  view, same as calendars.

- **Fetch errors keep the last good body.** On 404 / 5xx / DNS / timeout the
  existing body is left untouched; the error shows only in the in-memory
  status. A flaky feed never blanks a good note and never churns history.
  Mirrors `cal_sync`'s progressive-fill resilience.

- **Live notes refuse MCP body edits.** Reuse the private-note boundary
  pattern: `update_note_body` is refused for any note with a `source`
  (the LLM's edit would be clobbered on the next refresh / file open). The
  LLM may still read, rename, move, delete. `private` and `source` are
  independent flags.

- **URL→text pipeline: `ureq → dom_smoothie → html2text`.** Verified
  end-to-end (below). `dom_smoothie` (maintained Rust port of Mozilla
  Readability) is in v1: a clear win on articles, neutral on aggregator
  pages, one sync crate, lib stays tokio-free.

## Pipeline, proven

Throwaway prototype run against live pages on 2026-05-17:

| Page | HTML in | raw html2text | dom_smoothie → html2text |
|---|---|---|---|
| HN front page | 35 KB | 31.8 K chars | 34.5 K chars (neutral) |
| Rust blog article | 16.7 KB | 6.0 K chars (nav+footer) | **4.4 K chars, clean prose, title auto-extracted** |

`html2text` output is genuinely readable: wrapped text, headings kept, links
as `[n]` references with a footnote list. Readability is a big win on real
articles (strips chrome, pulls the title) and harmless on aggregator pages
(HN's page basically *is* its content). Branch on `Content-Type`:

```
ureq GET ─┬─ text/markdown, text/plain  → body verbatim
          ├─ application/json           → pretty-printed fenced block
          └─ text/html                  → dom_smoothie (readability)
                                          → html2text → body
```

A `format` field on the source overrides the branch
(`auto` | `markdown` | `html` | `readable`) for servers that lie about
`Content-Type` or to force readability off.

**Out of scope for v1, by agreement** ("too big for now"): tight structured
extraction (e.g. HN *titles only*) — that is not "make this page readable"
but "select these elements", which needs per-source CSS selectors
(`scraper`). The verbose-but-readable HN digest the v1 pipeline already
produces is acceptable in the meantime. Also deferred: webhook *push*
(server endpoint that writes a note on POST), and a manual-refresh button
plumbed from the TUI to the server.

## Data model

`models.rs`:

```rust
pub enum SourceKind { File, Url }

pub struct Source {
    pub kind: SourceKind,
    pub location: String,   // absolute path, or URL
    pub format: String,     // "auto" | "markdown" | "html" | "readable" (URL only)
    pub refresh_secs: u64,  // 0 = use server default (URL only)
}

// Note / NoteMeta gain:
pub source: Option<Source>,
```

Stored as a small static sub-map on the note (`note.<id>.source`), written
once at create / on edit, never per-refresh — so it cannot bloat history.
For a **file** note the note's `body` Text object is left empty (or absent);
the body never goes in the doc. For a **URL** note the `body` Text object is
the synced rendered content.

No genesis change is needed: a `source` sub-map is created by exactly one
replica under an already-unique note id — same no-concurrent-seed argument
as `cal/<id>` and `mark/<char>`.

## Store API (`store.rs`)

- `set_note_source(id, Source)` / `clear_note_source(id)` — static pointer
  writes, ride sync like `set_note_folder`.
- `live_notes() -> Vec<(id, Source)>` — for the fetch loop and the manager.
- `refresh_note_body(id, &str)` — **guarded**: reads the current body,
  returns early without opening a transaction if it equals the new text;
  otherwise splices. The single safe write path for machine-driven bodies.
- `note` / `note_metas` carry `source` so the tree can mark live notes and
  MCP can flag them.
- MCP boundary (`server_mcp.rs`): `update_note_body` refused when
  `source.is_some()`, reusing the private-note refusal shape.

## File replica (TUI side)

New `src/live_file.rs`, sibling in spirit to `cal_sync` but no thread and no
network:

- On **open** of a note with `source.kind == File`:
  - path resolves → read the file, record its mtime, show its contents in
    the editor. The editor operates on file text, not an Automerge Text
    body, for this note.
  - path does not resolve → read-only placeholder:
    *"live file — not present on this device: `<path>`"*.
- On **save**: if the file's mtime changed since open (external / git
  edit), do **not** clobber — warn and let the user choose (v1: warn +
  keep-mine vs reload; the conflict is ordinary editor territory). Else
  write the buffer to the file.
- Tree / list: a marker glyph on live notes; the manager (below) lists them
  with last-read status.

## URL fetch (server side)

New `src/live_url.rs`, reachable from the `ccal-server` binary; shaped like
`cal_sync::refresh` (snapshot under a brief lock, network IO with the lock
released, `catch_unwind` around parsing, status published in memory):

- Per **doc** in the server registry, a `tokio::spawn`ed loop (the server is
  the async side; the lib stays tokio-free — the fetch+convert helpers are
  sync and run via `spawn_blocking`).
- Each cycle: `store.live_notes()` filtered to `Url`; for each, fetch with
  `ureq`, run the pipeline, call `refresh_note_body` (guarded), then
  `save`. The existing debounced saver + change-broadcast publish it live.
- Errors → in-memory `LiveStatus` + server log; body untouched.
- Interval: `config.rs` gains `live_refresh_secs()`
  (`$CCAL_LIVE_REFRESH` > `[server] live_refresh_secs` > 900, floored),
  copying `calendar_refresh_secs()` verbatim; per-note `refresh_secs`
  overrides when non-zero.

## TUI surface

- A **Live Sources** manager view (modelled on the calendar manager):
  add / list / remove sources, see per-source status (file: present?
  last read; url: last ok, last error, bytes).
- Add-source flow: pick folder + title + kind + location (+ format for
  URL). Creates the note and its `source` pointer.
- A marker on live notes in the folder tree.

## Crates

`Cargo.toml` gains `dom_smoothie` and `html2text` (both sync, pure Rust, no
JS engine — lib stays tokio-free; used from the server binary's blocking
fetch path). `ureq` is already a dependency.

## Phasing

- **P1 — File replica.** models/store pointer, `live_file.rs`, on-open
  read / on-save write + mtime warn, placeholder for missing file, manager
  view, tree marker, MCP refusal. No network, no HTML — the small one.
- **P2 — URL notes.** `live_url.rs` server task, the verified pipeline,
  guarded splice, in-memory status, config interval, `format` override.
- **P3 — deferred.** Structured extraction (HN titles-only via `scraper` +
  per-source selector), webhook push endpoint, manual refresh plumbed
  TUI→server, two-way *file* beyond the v1 mtime-warn (richer merge UI).
