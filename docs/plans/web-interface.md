# Web interface plan — browser as a true Automerge peer (path A)

Status: planned. Approach chosen 2026-05-17: a **separate** PWA in
`../ccal-web` (its own repo — TypeScript + React + `@automerge/automerge`
WASM), talking to `ccal-server`'s existing raw sync socket with the
*minimal* possible server change. The web client is a full, offline-capable
peer — installable, works with no server, reconnects and converges when the
network returns. This file lives here (not in `../ccal-web`) because the
server-side change and the cross-implementation contract are the parts that
touch this repo; `../ccal-web` is greenfield against that contract.

## Goal & scope

A phone-first web app that views and edits notes and todos with the **same
semantics as the TUI** — because it *is* the same Automerge document, run in
the browser. Not a thin server-rendered view (that was path B, rejected):
the doc lives in the browser, edits are local and conflict-free, and the
WebSocket only carries Automerge sync bytes. Offline is therefore not a
feature to build but a property that falls out of running the CRDT
client-side; the work is making the *transport, storage and reconnection*
robust, not making merge work.

In scope: notes (tree, title, body, move, create, delete), todos (list,
add, reorder, edit, delete), live convergence with TUI/MCP edits.
Out of scope for v1: History/checkpoint time-travel (server-owned,
single-writer — see Decisions), calendars, live URL/file notes
(`docs/plans/live-notes.md`), the private-note *toggle* (read/keep only —
see Decisions).

## Decisions

- **Browser is a fully-trusted peer, equivalent to the TUI — not an
  MCP-grade redacted client.** A true Automerge peer receives the *whole*
  document, including private-note bodies in plaintext, exactly as the TUI
  does (privacy is a TUI-only display toggle; redaction is enforced *only*
  at the MCP boundary in `server_mcp.rs`). Path A structurally cannot
  provide MCP-style redaction on the web. This is acceptable **only** under
  the established trust model: an owned device, the server reached over
  Tailscale, the same trust class as the laptop running the TUI. The web
  client renders the `private` flag (a lock marker, body shown) and must
  **not** offer the `p` toggle — privacy stays a TUI action so there is one
  authority for it. Consequence to accept explicitly: offline support means
  **private-note plaintext is persisted at rest in the browser's IndexedDB**
  (mitigated only by device-level encryption — it is your phone).

- **Minimal server change: one auth accommodation, nothing else.** Browsers
  cannot set an `Authorization` header on a `WebSocket`, but `sync_handler`
  rejects the upgrade without `Bearer <token>`. The *only* server change is
  to also accept the token via the `Sec-WebSocket-Protocol` subprotocol
  (preferred — kept out of access logs, unlike a `?token=` query string,
  which is the documented fallback for clients that can't set a
  subprotocol). The header path is untouched, so the TUI and a future
  `automerge-swift` client are unaffected. `serve_peer`/`flush` and the wire
  format do **not** change — the socket stays "raw `automerge::sync::Message`
  bytes, nothing else", and that line in the binary's doc-comment gains a
  note about the subprotocol auth.

- **No History / checkpoints from the web.** `ROOT["checkpoints"]` is a
  deliberate *single-writer* (server-only) structure; a JS peer writing it
  is unsound (see `automerge-store-design`). The web client ignores it
  entirely — time-travel/restore stays a TUI+server concern. v1 web is
  notes+todos only.

- **The web client must never seed genesis.** "Two peers each seed their own
  `notes`/`todos`/`schema` ROOT map" is *the* genesis hazard called out in
  `store.rs` and `automerge-store-design`. The client adopts an existing
  synced doc only; it must never create those objects and must refuse to
  render (show a "syncing…" state) until the first sync from the server has
  populated the doc. A brand-new install with no server reachable and no
  cached doc has nothing to show — correct, not a bug.

- **Schema is documented once here and conformance-tested, because path A
  duplicates it.** The doc shape now has a second implementation (TS) with
  no Rust type system tying it to `store.rs`. The mitigation is a
  cross-implementation **conformance fixture** (see Phasing P1): the
  authority for the schema is `store.rs`'s doc-comment, restated below;
  every change to it (e.g. the `source` field from `live-notes.md`) is a
  two-sided change with a fixture update.

- **React, per request, despite bundle weight.** Pay the size with
  `preact/compat` aliasing only if mobile bundle size proves a problem in
  testing — not pre-emptively. State is a reactive projection over the local
  Automerge doc, re-derived on each applied change; React renders that
  projection, it never holds doc state of its own.

## Server change (this repo) — the whole of it

`src/bin/ccal-server.rs`, `sync_handler`: before the existing
`bearer(&headers)` check, also derive a candidate token from the
`Sec-WebSocket-Protocol` request header (a `ccal.bearer.<token>` convention)
and, as a fallback, a `?token=` query param; accept the upgrade if **any**
source matches `app.token`. When the subprotocol form is used, echo the
selected subprotocol back on the response per the WS spec. The bearer
helper is reused; the rejection shape, the `docid` validation, and
everything downstream are unchanged. Update the wire-protocol doc-comment.
That is the entire server delta — no new route, no new module, no change to
the sync loop, the saver, or MCP.

## Document schema (the cross-implementation contract)

Authority: `src/store.rs` module doc-comment. Restated for `../ccal-web`:

```
ROOT (Map)
  schema      : Int                       // currently 1
  notes       : Map id -> {
                  title    : Str
                  folder   : List<Str>    // path array; folders are derived
                  body     : Text         // char-CRDT
                  created  : Int
                  modified : Int
                  private  : Bool         // optional; render-only on web
                  source   : Map          // optional; live-notes.md, ignore in v1
                }
  todos       : Map id -> { text:Str, order:F64, created:Int }
  cal/<id>    : Map { ... }               // ignored by web v1
  mark/<char> : Str                       // ignored by web v1
  checkpoints : Map                       // SERVER-ONLY; never read/write from web
```

Derived, not stored — the TS must reproduce these from `store.rs`:
- **Folder tree** from each note's `folder` path array; an empty folder
  simply ceases to exist (no folder entity). Mirror `folder_tree()`.
- **Todo order** via the fractional `order: F64` index; reorder = pick a new
  fractional key between neighbours (mirror `swap_todos`/reorder), never a
  full renumber.
- Edits set `modified = now_ms()` and splice `body` minimally (the
  CodeMirror binding does this; see P3).

## Client stack — the one open fork (resolve in P0)

`ccal-server` speaks **raw** `automerge::sync::Message` bytes with the doc
id in the URL path and *no envelope*. `@automerge/automerge-repo`'s stock
WebSocket adapter speaks the *automerge-repo* sync protocol (its own CBOR
handshake + message envelopes) — it will **not** talk to this server. Two
ways forward, decided by the P0 spike:

1. **Bare `@automerge/automerge` + hand-rolled glue.** We write: a WS client
   that runs the raw sync loop mirroring `serve_peer` (generate/receive sync
   message against a local `SyncState`), an IndexedDB persistence layer
   (load on boot, debounced save on change — the client-side analogue of the
   server's `saver`), and reconnect-with-backoff. Most code; zero server
   compromise; exact protocol match. *Recommended* — it keeps the server
   minimal, which is the stated constraint.
2. **`@automerge/automerge-repo` + a custom `NetworkAdapter`.** Buys
   IndexedDB storage, reconnection, and `@automerge/automerge-repo-react-hooks`
   for free, but the custom adapter must bridge repo's per-doc sync engine
   onto the server's raw single-doc protocol — the trickiest, least-trodden
   piece, and a moving target across repo versions.

Recommendation: **option 1.** The hand-rolled sync/storage/reconnect is
~a few hundred lines, well-specified by `serve_peer` + the server's
`saver`, and avoids betting the project on an adapter fighting
automerge-repo's internals. Revisit only if P0 shows the raw loop is
fragile across the JS/Rust version pair.

## Phasing

- **P0 — interop spike (throwaway, gating).** A ~30-line static page using
  bare `@automerge/automerge` opens `/sync/<docid>` (token via subprotocol),
  completes a sync against a running `ccal-server`, round-trips one note
  edit both directions, and converges with a TUI on the same doc. Output:
  the exact proven `@automerge/automerge` ↔ `automerge 0.9` version pair,
  and the client-stack fork resolved. **If this fails, path A is not
  viable** — stop and revisit B. Nothing below starts until P0 is green.

- **P1 — schema module + conformance harness.** In `../ccal-web`: the typed
  doc-shape module (read/derive folder tree, read todos in order, the
  genesis-refusal guard, `checkpoints`/`cal`/`mark` ignored). In *this*
  repo: a test that has the Rust `Store` write a known fixture doc which the
  TS reads and asserts, and vice-versa (TS-written doc opened by `Store`),
  so the two implementations cannot silently drift. This harness is the
  standing guarantee for every future schema change.

- **P2 — server auth accommodation.** The single `sync_handler` change
  above, with a test for subprotocol, query-fallback, and the unchanged
  header path. Self-contained; can land independently of the webapp.

- **P3 — editing core.** Notes: create/rename/move/delete, body editing via
  **CodeMirror 6 + `@automerge/automerge-codemirror`** (character-granular
  splices = the TUI's merge behaviour, for free). Todos: add/edit/delete/
  reorder against the fractional index. React renders the projection;
  mutations go straight to the local doc.

- **P4 — offline/PWA + transport.** The raw-sync WS client (mirrors
  `serve_peer`), IndexedDB persistence (boot-load + debounced save, the
  client-side `saver`), reconnect-with-backoff that resyncs from local
  have-deps on return (the protocol re-derives — no persisted per-peer
  state, same as the server). Service worker + `manifest.json` +
  add-to-home-screen; app shell cached so a cold offline launch shows the
  last synced doc and accepts edits that flush on reconnect.

- **P5 — phone polish.** Touch targets, virtual-keyboard-aware editor,
  large-corpus list virtualization, the `private` lock marker (no toggle).

## Risks carried

- **Format interop** (P0-gated) — the single largest unknown; the
  "language-neutral wire protocol" claim is unproven against JS.
- **Schema duplication** — mitigated, not eliminated, by the P1 conformance
  harness; every schema change is now two-sided forever.
- **Private plaintext at rest** in IndexedDB on the phone — accepted under
  the device-trust model; recorded here so it is a decision, not a
  surprise.
- **Genesis seeding** — a JS bug that creates `notes`/`todos` on an empty
  doc corrupts the shared corpus; the render-after-first-sync guard is
  load-bearing and must be tested offline-cold-start.
