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
- The **sync algorithm** in `src/sync_client.rs`: push-everything-owed loop,
  drain-and-apply loop, exponential backoff, the lock discipline (hold the
  `Store` lock only across generate/receive/save, never across network IO),
  and the dirty-flag handoff to the UI. This logic is settled and commented;
  only its *transport* is ported.

## Refactor required before the app

`sync_client.rs` is TUI-binary-private and built on **blocking
`tungstenite` + one `std::thread`** to keep the lib tokio-free. On iOS under
Dioxus an async WebSocket is the natural fit. The "lib stays tokio-free"
rule is preserved by keeping the async transport in the *app crate*, not the
lib — exactly as `ccal-server` keeps tokio in its binary.

1. **Workspace conversion.** Today `ccal` is a single crate. Convert to a
   Cargo workspace:
   - `ccal-core` (or keep `ccal` as the lib) — the existing tokio-free lib
     (`models`, `store`). Unchanged.
   - `ccal` binaries (TUI, `ccal-server`, `import-bear`) — unchanged, depend
     on the lib by path.
   - `ccal-mobile` — new Dioxus crate, depends on the lib by path.
   This keeps the lib's no-async guarantee intact and lets the mobile crate
   bring its own async stack without polluting the TUI build.
2. **Extract the sync loop shape.** Factor the generate/receive/backoff
   structure out of `sync_client.rs` into something both transports can
   drive. Two acceptable shapes — pick during implementation:
   - a small state-machine type in the lib that takes/returns message bytes
     and has no IO (cleanest; testable; both clients own their socket), or
   - leave `sync_client.rs` as the desktop impl and write a parallel
     `mobile_sync` in `ccal-mobile` that mirrors its loop with
     `tokio-tungstenite`. Less DRY, zero risk to the working TUI.
   Default to the parallel impl unless the state-machine extraction proves
   clean — the TUI sync is working and shipped; don't regress it for tidiness.

## Mobile architecture

- **Local replica.** `Store::open_at(<app sandbox>/ccal.automerge)`. On
  iOS that path is the app's Documents/Application Support directory
  (resolve via the platform dirs, not `Store::open`'s desktop default).
- **Persistence.** Call `Store::save()` after every local mutation and after
  any merged remote change (mirrors `sync_client.rs` line ~144). On launch,
  `open_at` reloads the saved blob, so notes captured offline survive and
  sync later. Genesis-seeding in `Store` already guarantees client/server
  replicas share an ancestor so first sync converges.
- **Sync task.** A `tokio` task (not OS thread) running the ported loop
  against `tokio-tungstenite`, `Authorization: Bearer <token>` header,
  `ws://<host>:8787/sync/ccal`. Same backoff and status reporting. Store
  shared with the UI via `Arc<Mutex<Store>>`; lock held only across
  generate/receive/save.
- **UI → sync handoff.** Reuse the `dirty: AtomicBool` + `status:
  Mutex<String>` handle pattern. Dioxus signals subscribe to it: on dirty,
  re-read `notes()`/`note_metas()`/`todos()` and rebuild views.
- **Config.** Sync URL + bearer token. v1: a minimal settings screen,
  token stored in the **iOS Keychain** (not plaintext prefs). Host/URL in
  app storage. No discovery — user pastes the tailnet URL once.

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

1. **Workspace split** — lib extracted, TUI/server/import build unchanged,
   CI/tests green. No behaviour change.
2. **`ccal-mobile` skeleton** — Dioxus app builds and runs in the iOS
   simulator, opens a `Store` at the sandbox path, renders a hardcoded
   notes list. No sync.
3. **Sync port** — async sync task against `ccal-server` over the tailnet;
   bidirectional convergence verified (add on TUI → appears on phone and
   vice versa); offline capture survives relaunch and syncs on reconnect.
4. **Screens + config** — all six screens, Keychain token storage,
   status banner.
5. **Device + polish** — real-device install on the tailnet, signing,
   status-message coverage for the "Tailscale down" case.

## Risks / open questions

- **Dioxus mobile maturity (0.6/0.7).** Expect first-build yak-shaving
  (signing, simulator wiring), smooth after. Acceptable for the scope.
- **Tailscale UX.** If the tailnet is down the app must degrade to a clear
  "offline, capture still works" state, not hang or error. The existing
  backoff handles reconnection; the UI must surface it honestly.
- **State-machine extraction vs parallel impl.** Resolve in Milestone 3;
  bias to not regressing the working TUI sync.
- **Keychain access from a Dioxus/Rust app on iOS.** Validate the crate
  path for Keychain early (Milestone 4); fall back to encrypted app
  storage if it's painful, but never plaintext.
