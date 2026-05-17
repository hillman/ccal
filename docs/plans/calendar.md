# Calendar plan — ICS subscriptions, read-only

Status: **implemented** 2026-05-17. Decisions made 2026-05-17.

Implementation notes (where it differs from the plan below): subscriptions
are stored as a per-calendar `ROOT["cal/<uuid>"]` **Map** (url/name/created),
not a scalar string — same convergence reasoning (unique key, single
creating replica, no shared parent container, no genesis change) but
idiomatic to the existing notes/todos modelling. Modules landed as
`ccal::calendar` (pure parse+expand, lib) and `src/cal_sync.rs` (TUI-private
fetch thread). `webcal://` URLs are accepted and normalised to `https://`.
v1 keeps a single agenda view (Today + next 7 days) plus an `m` manage
sub-view for add/delete/status; no per-calendar colour.

## Goal & scope

A read-only "Calendar" tab alongside Todos and Notes. You paste a calendar
URL, it refreshes on a timer (or on demand), and you get an agenda for today
plus a dated "this week" list.

In scope:

- Add a calendar by pasting an **ICS subscription URL** (Calendar tab, one
  keypress).
- Remove a calendar.
- Background refresh every N minutes (default 5) and an on-demand refresh
  key.
- Today agenda: all-day events pinned, then timed events in order.
- This-week list: the next 7 days, grouped under weekday + date headers.
- Recurring events expanded correctly (RRULE/EXDATE), timezones resolved,
  all-day vs. timed handled.

Out of scope (v1, and likely forever): creating/editing/deleting events,
RSVP/attendees/reminders, free-busy, CalDAV, OAuth, month view, colour
customisation UI, notifications, mobile (the pure parser is reusable, but
the mobile plan v1 explicitly excludes calendar).

## Decisions

- **ICS subscriptions only.** Google exposes a private *"Secret address in
  iCal format"* per calendar; Proton's published link is already ICS. Both
  your providers are covered by an authenticated-by-URL HTTPS `GET`. CalDAV
  is declared out of scope: Google CalDAV is OAuth2-only (no basic auth / no
  app passwords), which is a large amount of work for zero benefit to a
  read-only view.

- **Subscriptions sync; events do not.** The subscription *list* (the URLs
  you paste — tiny, user-authored, the same on every device) lives in the
  shared Automerge doc, so adding a calendar on the TUI propagates to other
  devices and is backed up by the server. The *events* are derived cache:
  each device fetches and expands ICS into a **local, non-synced** cache.
  This keeps the doc tiny and avoids unbounded Automerge history growth —
  syncing thousands of re-expanded occurrences every 5 minutes would hit the
  "pathological at scale" constraint this project already documents. Offline
  shows the last good local cache.

- **Subscriptions stored as scalar-per-ROOT-key, not a new container map.**
  Adding a new `ROOT["calendars"]` *object* would re-introduce the exact bug
  `store.rs` documents: two replicas independently `put_object` an empty map
  with no common ancestor → permanent ROOT-key conflict, entries lost. Fixing
  that "properly" means changing `genesis_doc()`, which changes the genesis
  change bytes and **breaks the common ancestor for existing notes/todos**.
  We sidestep both: each subscription is a single scalar string under a
  unique key `ROOT["cal/<uuid>"]`. ROOT itself is universally shared (it is
  the document root, never `put_object`'d), keys are unique per calendar so
  there are no concurrent writes to the same key, and scalar values have no
  object-identity to diverge. No genesis change, no migration, converges
  trivially.

- **Fetching mirrors `sync_client`'s shape.** One blocking `std::thread`, no
  tokio in the lib, `Arc<Mutex<_>>` shared with the UI, lock held only
  briefly. `ureq` (blocking, rustls, tokio-free) does the HTTPS `GET`.

## What is reused unchanged

- `ccal::Store`'s open/save/sync facade and the existing single-doc sync
  path. Subscriptions ride the existing `/sync/ccal` connection for free —
  no second document, no second connection, no server change.
- The `sync_client.rs` thread *pattern* (spawn, `Arc<Mutex<Store>>`, brief
  lock discipline, a `Handle` the UI polls each tick) is the template for
  the new refresh thread. Not literally shared, same as the mobile plan.
- `models::new_id` / `now_ms`, the modal Tab/Mode/key-routing structure in
  `app.rs`, and the `ui.rs` tab/list/status rendering.

## New dependencies (deliberate, documented in Cargo.toml per repo convention)

Pure calendar semantics are genuinely the bulk of the work and unavoidably
need real date/time machinery — the codebase otherwise uses bare `i64`
epoch ms with no `chrono`:

- lib (pure, tokio-free, **no network**): an iCal *parser* (`icalendar` or
  `ical`) and the **`rrule`** crate, which is a full RFC 5545 recurrence
  engine (RRULE/RDATE/EXRULE/EXDATE, timezone-aware via `chrono` +
  `chrono-tz`, windowed iteration). Recurrence expansion is **delegated to
  `rrule`, not implemented here** — the parser yields the raw rule strings,
  `rrule` parses and expands them. `chrono`/`chrono-tz` come in transitively.
- TUI binary only: `ureq` (blocking HTTPS, rustls — keeps the lib network-
  free exactly like `sync_client` is binary-private, not in the lib).

## Module layout

- `src/calendar.rs` (lib, `ccal::calendar`) — **pure glue only**: parser
  crate → read each `VEVENT`'s `DTSTART` + raw `RRULE`/`RDATE`/`EXDATE`
  strings → hand them to a `rrule::RRuleSet` → ask for occurrences in the
  window → map each to a sorted `Occurrence`. No recurrence arithmetic, no
  timezone code, no RRULE interpreter is written here. No I/O, no network,
  unit-testable against captured Google/Proton exports. Reusable by a future
  mobile client.
- `src/cal_sync.rs` (TUI binary-private, like `sync_client`) — the refresh
  thread: read subscriptions from the store, `ureq` GET each (with
  `If-None-Match` / `If-Modified-Since`; honour `304`), call
  `ccal::calendar` to expand, publish into the shared local cache, set a
  `dirty` flag and per-calendar status. Force-refresh via a trigger the UI
  pokes.
- `src/store.rs` — add `add_calendar` / `remove_calendar` / `calendars()`
  over the `cal/<uuid>` scalar keys. Schema int stays `1` (no structural
  change). No `genesis_doc` change.
- `src/models.rs` — `Calendar { id, url, name, colour, created }` (synced
  subscription) and the local-only view types `Occurrence { summary, start,
  end, all_day, location, calendar_id, calendar_name, colour }` and
  `CalStatus { last_fetch, last_error, etag }`.
- `src/app.rs` / `src/ui.rs` — `Tab::Calendar`, agenda + week rendering, the
  add/remove/refresh keys.

## Storage detail

- Key: `ROOT["cal/<uuid-v4>"]`. Value: a small serialised string (a
  `serde_json` object `{name,url,colour}` — `serde_json` is already a dep).
- `add_calendar(url, name, colour)`: `tx.put(ROOT, "cal/<id>", json)`.
- `remove_calendar(id)`: `tx.delete(ROOT, "cal/<id>")`.
- `calendars()`: iterate `doc.keys(ROOT)`, take those starting `cal/`, parse.
- `name` defaults from the ICS `X-WR-CALNAME` on first successful fetch if
  the user didn't type one; the URL is the secret so it is never shown in
  full in the UI.

## Refresh semantics

- Default interval 300s; `[calendar] refresh_secs` in `config.toml` (env
  `CCAL_CAL_REFRESH` per the existing precedence rule). On-demand: `r`.
- Expansion window: `[today − 1 day, today + 35 days]` — enough for "today"
  and "this week" with slack — capped per calendar to bound memory.
- Send `If-None-Match`/`If-Modified-Since` from the stored ETag/Last-Modified;
  on `304` keep the existing cache. On any failure keep the last good cache,
  record `last_error`, retry next interval (no aggressive backoff needed at a
  5-minute cadence).

## TUI

- Tab cycle: Todos → Notes → Calendar → Todos.
- Default view (read-only): a **Today** block (all-day events first, then
  timed `HH:MM–HH:MM  summary  · location`), then **This week** — the next 7
  days, each under a `Mon 19 May` header, empty days shown as `—`. `j/k`
  scroll the week.
- Keys: `a` add calendar (input prompt: paste URL; optional follow-up name,
  else `X-WR-CALNAME`), `d` delete the selected calendar from a small manage
  list, `r` refresh now.
- A per-calendar problem (e.g. `401`, parse error) surfaces in the manage
  list and as a one-line footer note; it never blanks the agenda.

## Risks

1. **Feeding `rrule` the right inputs.** Recurrence/timezone *math* is the
   `rrule` crate's job, not ours — the residual risk is narrower: handing it
   correct rule strings and getting all-day (`DTSTART;VALUE=DATE`) vs. timed
   vs. floating `DTSTART` right. Mitigation: keep the mapping in the pure
   `ccal::calendar` module and unit-test against *captured real* Google and
   Proton exports (they differ: line folding, `TZID` forms, `EXDATE`).
2. **Dialect quirks** between providers — handled by a tolerant parser and
   the captured-export tests above.
3. **Dependency growth** (chrono, chrono-tz, ical, rrule, ureq) — accepted
   and called out in `Cargo.toml` comments the way tungstenite/axum already
   are.
4. Subscription convergence without a genesis change — resolved by the
   scalar-per-ROOT-key design above; no residual risk.

## Implementation order

1. `ccal::calendar` pure parse+expand + tests on captured ICS fixtures.
2. `Store` calendar subscription accessors (scalar `cal/<uuid>` keys).
3. `cal_sync` refresh thread + `Handle`, wired into `App` like `sync`.
4. `Tab::Calendar` UI: today + week, then add/remove/refresh keys.
5. `[calendar]` config + docs in `config.example.toml` / `README`.
