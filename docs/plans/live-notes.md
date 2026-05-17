# Live notes plan — file replicas & server-fetched URL notes

Status: planned. Decisions made 2026-05-17 (URL→text pipeline verified with a
throwaway prototype against live pages — see "Pipeline, proven"). Decision #2
revised 2026-05-17: a URL note's body is **not** in the synced doc — server
cache + side-band push (see "URL body: out of the doc").

## Goal & scope

Two ways a note's content can be *driven from outside the doc*, from
`docs/ideas.md`:

1. **Live file replica** — a note backed by a local markdown file. Open it
   and you see/edit the file; close it and the file holds the truth.
2. **Live URL note** — a note whose body is a readable rendering of a fetched
   web resource (news, a webhook's markdown, an article), refreshed in the
   background by `ccal-server`.

Both halves share **one principle**: only the small user-authored `source`
pointer ever enters the Automerge doc; the body is *derived content* and
never syncs. This is the established `cal_sync` philosophy applied
uniformly, and it is what keeps refresh churn out of document history:

| | Pointer (path / url) | Body | Body store |
|---|---|---|---|
| **File note** | syncs | **not synced** | the local filesystem *is* the store |
| **URL note** | syncs | **not synced** | a `ccal-server`-side cache, pushed to clients side-band |

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

- **URL note: `ccal-server` is the single fetcher; the body never enters
  the doc.** A per-doc background task on the server fetches on an interval.
  Server-only because: it is always-on, it is a single well-known fetcher
  (no multi-device race, polite to rate limits), and it works while no TUI
  is open — the rationale that ruled out a per-client fetch. The rendered
  body is written to a **server-side cache** (in memory, persisted to
  `<data_dir>/<docid>.live/<noteid>` so it survives a restart), *not* to
  the Automerge document. Refreshes therefore generate **zero** document
  history — the failure mode that an in-doc body cannot escape (a genuinely
  live page legitimately changes every cycle, so a diff-and-skip guard does
  not save it; per-object history truncation is impossible in Automerge,
  and global compaction would destroy the History/checkpoint time-travel
  feature). If no server runs, URL notes simply do not refresh — consistent
  with the file case ("just use what's there").

- **The body reaches clients side-band, not as a doc change.** A new
  non-doc message on the existing sync websocket carries
  `(note_id, rendered_body, fetched_at)`; the server pushes it on connect
  (current cache) and whenever a fetch produces new bytes. Clients hold a
  local `live_bodies` cache keyed by note id and render a URL note from it;
  MCP `get_note` reads the same server cache. This reuses the
  change-broadcast *transport* (same plumbing the embedded MCP server
  rides) without putting anything in the CRDT. A client that has never
  received a body shows the file-note-style placeholder. Fetch status /
  last-ok / errors are **never** persisted; they live in an in-memory
  handle (a `LiveStatus`, shaped like `cal_sync::CalStatus`) surfaced in a
  manager view, same as calendars.

- **Diff-and-skip is still applied — now as a wire/notify guard.** If the
  freshly rendered text equals the cached body, no side-band push is sent
  and no client redraw is triggered. Cheap, and it keeps idle URL notes
  silent on the socket.

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
For **both** file and URL notes the note's `body` Text object is left empty
(or absent); a live body never goes in the doc. A file note's body is the
file on disk; a URL note's body is the server cache delivered side-band.

No genesis change is needed: a `source` sub-map is created by exactly one
replica under an already-unique note id — same no-concurrent-seed argument
as `cal/<id>` and `mark/<char>`.

## Store API (`store.rs`)

- `set_note_source(id, Source)` / `clear_note_source(id)` — static pointer
  writes, ride sync like `set_note_folder`.
- `live_notes() -> Vec<(id, Source)>` — for the fetch loop and the manager.
- **No `refresh_note_body` doc write.** A URL body never touches the
  Automerge doc, so there is no guarded-splice path. Instead the server
  owns a `LiveCache` (`note_id -> { body, fetched_at }`, in memory +
  `<data_dir>/<docid>.live/`); `cache.put(id, &str)` returns whether the
  bytes changed (the diff-and-skip guard) and, when changed, the server
  emits the side-band push.
- `note` / `note_metas` carry `source` so the tree can mark live notes and
  MCP can flag them.
- MCP boundary (`server_mcp.rs`): `update_note_body` refused when
  `source.is_some()`, reusing the private-note refusal shape; `get_note`
  for a URL note returns the `LiveCache` body, not the (empty) doc body.

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
  `ureq`, run the pipeline, `LiveCache::put`. If the bytes changed, push
  the side-band `(note_id, body, fetched_at)` message to connected clients
  (and invalidate the MCP read). No `store.save()`, no doc change, no sync
  message — the document is untouched.
- Errors → in-memory `LiveStatus` + server log; cached body untouched.
- Interval: `config.rs` gains `live_refresh_secs()`
  (`$CCAL_LIVE_REFRESH` > `[server] live_refresh_secs` > 900, floored),
  copying `calendar_refresh_secs()` verbatim; per-note `refresh_secs`
  overrides when non-zero.

## URL body: out of the doc (side-band channel)

The one new piece of protocol. The sync websocket currently carries only
Automerge sync messages; add a framed non-doc variant:

```
LiveBody { note_id: String, body: String, fetched_at: i64 }
```

- **Server → client, on connect:** after the initial Automerge sync, the
  server walks its `LiveCache` and sends one `LiveBody` per URL note the
  client can see. A fresh client is immediately whole.
- **Server → client, on change:** when a fetch cycle's `LiveCache::put`
  reports changed bytes, broadcast one `LiveBody`. Unchanged → nothing on
  the wire (the diff-and-skip guard, now a notify guard).
- **Client:** a `live_bodies: HashMap<NoteId, LiveCacheEntry>` beside the
  `Store`. A URL note renders from it; a miss renders the placeholder
  *"live url — not fetched yet"*. Never written back, never synced.
- **Persistence:** `<data_dir>/<docid>.live/<noteid>` on the server so a
  restart does not blank every URL note until the next cycle. The client
  cache is purely in-memory (rehydrated from the on-connect replay).
- **MCP:** `get_note` / `get_notes` for a URL note read `LiveCache`, not
  the empty doc body, so an assistant sees what a human sees.

This deliberately mirrors `cal_sync`'s "subscription syncs, fetched data is
a local cache" split, extended with a push so the *server* remains the sole
fetcher (the property a pure calendar-style per-client fetch would lose).

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
  `LiveCache` (in memory + `<data_dir>/<docid>.live/`), the `LiveBody`
  side-band message (on-connect replay + on-change push), client
  `live_bodies` cache + placeholder, MCP cache read, in-memory status,
  config interval, `format` override. **No doc write path.**
- **P3 — deferred.** Structured extraction (HN titles-only via `scraper` +
  per-source selector), webhook push endpoint, manual refresh plumbed
  TUI→server, two-way *file* beyond the v1 mtime-warn (richer merge UI).
