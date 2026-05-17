# Mobile app plan — Dioxus, Tailscale-dependent

Status: planned. Decision made 2026-05-17.

## Goal & scope

A low-friction capture-and-glance companion to the TUI. Explicitly **not** a
full client.

In scope:

- View notes (list by folder, read a note's body — read-only render).
- Add a note (title + body).
- View todos (list).
- Add a todo.

Out of scope (v1, and probably forever): note editing, folder management,
todo reordering/completion, calendar, the LLM features, settings beyond the
sync endpoint, Android (the codebase stays cross-platform-capable but only
iOS is built and tested).

## Decisions

- **Dioxus** for the UI. The whole point is reusing the Rust core; a native
  Swift client would mean reimplementing the (already-correct) store schema
  and sync loop a second time for no functional gain. `automerge-swift` is
  therefore not used and not relevant — Dioxus links the same `automerge`
  crate the rest of the repo already uses.
- **Background flush on backgrounding (tier 1 only).** A `beginBackgroundTask`
  window lets the existing sync thread finish flushing when the app is
  backgrounded, so a just-captured note reaches the server within seconds
  without reopening. This is a latency optimisation over the durability
  guarantee, not a replacement for it. Opportunistic `BGAppRefreshTask`
  (tier 2) is deferred post-v1; surviving a user force-quit is an explicit
  non-goal. Full reasoning in the Background sync section.
- **Tailscale-dependent.** The phone joins the tailnet via the Tailscale iOS
  app; the mobile client speaks plain `ws://` to `ccal-server` exactly like
  the TUI. No TLS, no server change, no reverse proxy. This matches the
  existing "Tailscale does auth+encryption at the network layer" design and
  is the lowest-engineering-cost option. The cost is borne by the user:
  Tailscale must be installed and logged in for sync to work. Offline
  capture still works (see persistence) and syncs when the tailnet is
  reachable.

## What is reused unchanged

- `ccal::Store` and `ccal::SyncState` — the entire data model and the sync
  facade (`generate_sync_message` / `receive_sync_message` / `save` /
  `open_at`, plus `notes` / `note` / `note_metas` / `create_note` /
  `set_note_*` / `todos` / `add_todo`). The mobile app is a new front-end
  over this lib; it never touches the `automerge` crate directly, same rule
  as `app`/`ui`/`sync_client`.
- The **sync client** `src/sync_client.rs` reused **near-verbatim**: spawned
  as a `std::thread` running blocking `tungstenite`, talking to
  `Arc<Mutex<Store>>`, with the existing push/drain loops, exponential
  backoff, lock discipline (hold the `Store` lock only across
  generate/receive/save, never across network IO) and the `dirty`/`status`
  handle the UI polls. This logic is settled and commented; it is not
  rewritten.

## Crate layout (no sync rewrite needed)

Earlier framing here ("async port required to honour a tokio-free rule")
was wrong, and is corrected:

- The lib being **sans-IO / tokio-free** is a clean boundary worth keeping —
  it's *why* this app is cheap (reuse doc + sync, only the process differs).
  It is orthogonal to sync-vs-async and requires no work to preserve.
- Dioxus on native/mobile **brings its own async runtime (tokio) anyway**, so
  a "tokio-free mobile app" was never on the table and must not drive any
  decision. There is nothing to protect in the mobile crate.
- Therefore the mobile app **reuses `sync_client.rs` as-is on a
  `std::thread`**. One blocking sync thread coexists fine alongside Dioxus's
  runtime. No async port, no state-machine extraction, no constraint to
  design around. (An async `tokio-tungstenite` port is possible later purely
  for code aesthetics; it buys nothing functional here — notably *not*
  background sync, see below — so it is explicitly out of scope.)

Workspace split — still worth doing, for an honest reason: keep Dioxus's
heavy dependency tree out of the TUI/server build, not to preserve any
tokio-free guarantee.

- Keep `ccal` as the tokio-free lib (`models`, `store`) — unchanged.
- TUI / `ccal-server` / `import-bear` binaries — unchanged, depend on the
  lib by path.
- `ccal-mobile` — new Dioxus crate, depends on the lib by path, and pulls
  `sync_client`'s logic in (lift it from binary-private to a shared module,
  or copy it — it is ~100 lines and stable).

## Mobile architecture

- **Local replica.** `Store::open_at(<app sandbox>/ccal.automerge)`. On
  iOS that path is the app's Documents/Application Support directory
  (resolve via the platform dirs, not `Store::open`'s desktop default).
- **Persistence.** Call `Store::save()` **synchronously, on the write path,
  before the UI handler returns** — never debounced or deferred for capture
  actions. `Store::save()` is already atomic (temp file + rename,
  `store.rs:118`), so an app kill mid-write cannot corrupt the blob. On
  launch, `open_at` reloads the saved blob. Genesis-seeding in `Store`
  guarantees client/server replicas share an ancestor so first sync
  converges.
- **Sync thread.** `sync_client.rs` spawned as a `std::thread` (the existing
  blocking `tungstenite` code, unchanged): `Authorization: Bearer <token>`
  header, `ws://<host>:8787/sync/ccal`, existing backoff and status
  reporting. Store shared with the UI via `Arc<Mutex<Store>>`; lock held
  only across generate/receive/save.
- **UI → sync handoff.** Reuse the `dirty: AtomicBool` + `status:
  Mutex<String>` handle pattern. Dioxus signals subscribe to it: on dirty,
  re-read `notes()`/`note_metas()`/`todos()` and rebuild views.
- **Config.** Sync URL + bearer token. v1: a minimal settings screen,
  token stored in the **iOS Keychain** (not plaintext prefs). Host/URL in
  app storage. No discovery — user pastes the tailnet URL once.

## "If I add a note then close the app, does it sync?" — durability vs. sync

These are two separate guarantees. Only the first must be absolute.

1. **Durability (the note is not lost).** Solved locally and independently
   of the network. `create_note` / `set_note_body` mutate the doc; the UI
   handler then calls `Store::save()` synchronously before returning. That
   write is atomic. So the moment the "save/done" action completes, the note
   is durably on disk in the app sandbox — even if the user immediately force
   -quits, iOS kills the suspended app, or there has never been a network
   connection. Nothing is buffered only in memory.
2. **Sync propagation (the note reaches the server).** Eventually consistent,
   and *that is fine*. If the app closes before the sync thread flushes the
   change, the change is still durably in the local Automerge doc. On the
   next launch, `open_at` reloads it and the sync thread's first
   `generate_sync_message` against a fresh `SyncState` produces exactly the
   outstanding change; it converges on the server. No data is lost — sync is
   only *delayed* to the next launch-and-connect.

The only real failure mode is a bug that lets the UI return *before*
`Store::save()` completes (e.g. a "fire and forget" save, a debounce, or
saving on a background tick). The rule that prevents it: **capture actions
save synchronously and the UI must not signal success until `save()`
returns `Ok`.** This is a hard requirement, called out again in Milestones
and tested explicitly (kill the app immediately after add → relaunch →
note present → later syncs).

Note iOS gives no reliable "about to be killed" callback for a suspended
app, so there is deliberately **no** "flush to server on background" step —
relying on one would be the wrong design. Local save-on-write is the whole
durability story; the server is caught up opportunistically.

## Background sync

Goal: shorten the window between capturing a note and it reaching the
server, without requiring the user to reopen the app. This is a **latency
optimisation, not a safety mechanism** — durability above is what guarantees
nothing is lost; this only affects *how soon* it propagates.

iOS dictates three tiers, and they must not be conflated:

1. **Backgrounded just after a capture (in scope, v1).** When the app moves
   to the background, take a UIKit `beginBackgroundTask` window (iOS grants
   on the order of ~30s — not contractually guaranteed, but far more than
   enough to flush a small Automerge sync over the WebSocket). Keep the
   existing sync thread running until it reports caught-up or the window is
   about to expire, then end the task. No new sync logic — this is a
   lifecycle hook around the thread that already exists. Covers the everyday
   case ("jotted an idea, switched to Messages") and gets it to the server
   within seconds.
2. **Left suspended for hours/days, never reopened (best-effort, post-v1,
   optional).** A `BGAppRefreshTask` registered with `BGTaskScheduler` that
   opens the connection, drains, and closes. iOS chooses if/when to run it
   on its own schedule; it may be delayed for hours or never run for a
   rarely-used app, and the user can disable Background App Refresh. Documented
   as best-effort only; **must never be presented or relied on as a delivery
   guarantee.** Deferred until after v1.
3. **Force-quit from the app switcher (explicit non-goal).** iOS runs no
   code from a user-terminated app until it is manually reopened. No
   mechanism changes this; not `beginBackgroundTask`, not BGTaskScheduler.
   After a force-quit, the captured note syncs on the next manual launch
   (durability still holds — it is on disk). This limitation is stated, not
   worked around.

Silent/remote push is explicitly rejected: the device holds the new data,
so push solves the wrong direction, and APNs infrastructure is grossly
disproportionate for a personal Tailscale tool.

Implementation note: `beginBackgroundTask`/`BGTaskScheduler` are UIKit APIs
Dioxus does not expose, reached via `objc2` FFI plus the Background Modes
capability (and `Info.plist` task identifiers for tier 2). The tier-1 shim
is small; tier 2 is fiddlier, which is part of why it is deferred. Tailscale
must still be up for a background-launched sync to succeed, and the app
cannot prompt for it from the background — a silent no-op in that case is
acceptable (durability covers it).

## Screens (v1)

1. **Notes list** — `note_metas()` grouped by folder. Tap → note view.
2. **Note view** — read-only render of `note(id).body`. No editor.
3. **New note** — title + body fields → `create_note` then `set_note_body`.
   A plain `textarea`; no vim, no edtui (that's a TUI concern).
4. **Todos** — `todos()` list (read-only display).
5. **New todo** — single field → `add_todo`.
6. **Status** — connection state from the sync handle in a footer/banner
   ("Synced" / "Sync: offline, retrying…" / "Sync: Tailscale unreachable").

Navigation: a 2-tab shell (Notes / Todos) plus modal "add" sheets. Keep it
boring.

## Build & tooling

- `dx` (Dioxus CLI), Xcode + iOS SDK, an Apple developer signing identity
  for device installs (simulator needs none).
- Iterate in the iOS simulator via `dx serve --platform ios`; periodically
  smoke-test on a real device on the tailnet (simulator can use the Mac's
  Tailscale; device needs the Tailscale iOS app — test that path early).
- Rendering is WebView-based (Dioxus mobile uses wry). Fine for these
  list/text screens; do not expect native UIKit feel.

## Milestones

1. **Workspace split** — lib stays as-is, TUI/server/import build unchanged,
   CI/tests green. No behaviour change. (Dependency hygiene only.)
2. **`ccal-mobile` skeleton** — Dioxus app builds and runs in the iOS
   simulator, opens a `Store` at the sandbox path, renders a hardcoded
   notes list. No sync.
3. **Durability** — add-note path calls `Store::save()` synchronously;
   explicit test: add a note, immediately kill the app, relaunch, note is
   present. This is verified *before* sync exists, to prove capture safety
   does not depend on the network.
4. **Sync wired up** — `sync_client.rs` thread against `ccal-server` over the
   tailnet; bidirectional convergence verified (add on TUI → phone and vice
   versa); a note captured offline (or with the app killed pre-flush) syncs
   on the next launch+connect.
5. **Screens + config** — all six screens, Keychain token storage,
   status banner.
6. **Background flush (tier 1)** — `objc2` shim taking a
   `beginBackgroundTask` window when the app backgrounds; existing sync
   thread runs until caught-up or the window is about to expire, then the
   task is ended. Test: add note, background the app (not force-quit),
   confirm it reaches the server without reopening. (Tier 2 BGAppRefreshTask
   remains out of v1.)
7. **Device + polish** — real-device install on the tailnet, signing,
   status-message coverage for the "Tailscale down" case.

## Risks / open questions

- **Dioxus mobile maturity (0.6/0.7).** Expect first-build yak-shaving
  (signing, simulator wiring), smooth after. Acceptable for the scope.
- **Tailscale UX.** If the tailnet is down the app must degrade to a clear
  "offline, capture still works" state, not hang or error. The existing
  backoff handles reconnection; the UI must surface it honestly.
- **Keychain access from a Dioxus/Rust app on iOS.** Validate the crate
  path for Keychain early (Milestone 4); fall back to encrypted app
  storage if it's painful, but never plaintext.
