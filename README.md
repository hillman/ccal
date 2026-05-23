# CCAL Reborn

A fast terminal app for **markdown notes and todos** with **offline-first,
multi-device sync** (Automerge CRDT) and an **optional MCP server** that lets
an LLM assistant help organise your notes — safely. Single static Rust
binary, no runtime dependencies. There's also a read-only **Calendar tab**
that subscribes to ICS feeds — deliberately secondary, just today + the week
at a glance.

## Features

- **Notes & todos in a terminal.** Markdown notes in a folder tree and an
  ordered, reorderable todo list, in a snappy [ratatui](https://ratatui.rs)
  UI. One static binary, nothing to install alongside it.
- **Tagged todos & filtering.** Give todos free-text tags, multi-select
  several at once and tag them in a batch, then filter the list to a single
  tag with tab-completion. An active filter is shown prominently and is
  auto-applied to new todos so they don't vanish from the view; `Esc`
  clears it. Tags sync and time-travel like everything else.
- **Folders are derived, not managed.** A note's folder is just its path —
  create, move and rename folders implicitly by filing notes; folder rename
  rewrites a whole subtree at once. No empty-folder bookkeeping.
- **Vim note editor.** Modal editing via [edtui](https://github.com/preiter93/edtui)
  with markdown syntax highlighting; app keys only fire in Normal mode, so
  typing never triggers a command.
- **Live preview pane.** The Notes tab's right pane shows the selected
  note's body (or a folder's contents) as you move the cursor — read
  things at a glance without opening the editor.
- **Jump-anywhere navigation.** Tabs are numbered `[1]`–`[4]`; press the
  number to go straight there from any screen. Plus Vim-style **global
  note bookmarks**: `gm{key}` bookmarks the selected note, `g{key}`
  reopens it from anywhere — and bookmarks sync across devices like
  everything else.
- **Offline-first sync, no conflicts.** The store *is* a CRDT (Automerge):
  every device works fully offline and converges automatically, with
  **character-level merge** of concurrent edits to the same note — edits
  are never lost and there are no conflict markers.
- **Run-your-own sync server.** `ccal-server` is a tiny always-on peer:
  point any number of clients at it; it merges, **rebroadcasts changes
  live**, and doubles as a free plaintext backup. No database, no schema,
  no migrations. Built to sit behind Tailscale (still bearer-token gated).
  The wire protocol is language-neutral (raw Automerge sync messages) so a
  future iOS client speaks it unchanged.
- **Optional MCP server for LLM assistants.** Opt-in, embedded in
  `ccal-server` over HTTP. Claude (or any MCP client) can list, search,
  read, create, edit, move, rename and delete notes & todos — 18 tools —
  and because the edits ride the same sync broadcast, they appear **live**
  in every open ccal client.
- **Web app (browser + PWA).** `ccal-server` serves an installable,
  offline-capable web app — the same notes & todos, phone-first — **embedded
  in the binary**. The browser is a real Automerge *peer* over the same sync
  socket, so edits are local, conflict-free and work offline (the doc is
  cached in IndexedDB). No separate web host.
- **Checkpoints & time-travel.** Automerge keeps the whole history, so
  snapshots are essentially free. Create named checkpoints; roll the whole
  corpus back to any checkpoint *or any point in the edit log*. Restore is
  itself a forward change, so it syncs cleanly to every device. A
  **History tab** browses the timeline and previews exactly what a restore
  would change before you commit to it.
- **Private notes.** Mark a note private to hide its body from the LLM
  (reads redacted, body-edits refused, never matched by search) while
  still letting it rename/move/organise around it. Enforced at the MCP
  boundary — your own devices still see everything; nothing is encrypted,
  and the assistant can't un-private a note.
- **Full-text search.** `/` searches titles, folder paths and bodies
  live; the assistant gets the same as a privacy-respecting `search_notes`
  tool.
- **Calendar (read-only).** Subscribe to ICS URLs — Google's per-calendar
  "Secret address in iCal format" and Proton's published-calendar link both
  work (no OAuth, no CalDAV). The Calendar tab shows today's agenda then the
  next 7 days; recurring events (RRULE/EXDATE) and timezones are expanded
  properly. Subscriptions sync like notes/todos; the fetched events are a
  per-device local cache refreshed every 5 min (or on demand), so the doc
  never bloats. `a` add · `r` refresh · `m` manage/delete.
- **Bear import.** One-shot `import-bear` reads the Bear app's SQLite
  read-only and maps nested tags to nested folders.
- **Configure your way.** Optional TOML config or `CCAL_*` env vars (env
  wins) with sensible defaults; no config at all = standalone, same code
  path.

## Why?

Way back in the day, when I was at university, myself and a friend made a curses based calendar/todo list app called Ccal.  We loved it and it had a niche community of people who used it, it was fun.  But work got in the way and it died.  I thought, in this age of LLM assisted side projects, where you can get much more done with the limited time you have, that I'd have a go at ressurecting it in Rust.  It's a spiritual successor, not a re-build. 

It's for notes, todos and calendar stuff.  Despite the name, the calendar bit is actually secondary: it just pulls in today's events from other calendars (read-only ICS subscriptions) for a glance at today and the week ahead.  It isn't a fully fledged calendar, and won't be.

I've built it in Rust for a nice quick UI and easy executable builds.  It uses Automerge as both the storage format and the sync protocol, which is what makes the self-hosted sync server (and the live LLM integration) work.

## Editor

It's using edtui for the note editor - that uses vim bindings, because I love vim and have it in my fingers.  Sorry, it's an opionated choice.  It'd be easy enough to implement a config flag for a simpler edit component though if anyone wants it. PR welcome.


## Navigation & bookmarks

Getting around is keyboard-only and designed so you never have to tab
through things you don't want.

**Tabs.** The tab bar is numbered — `[1] Todos`, `[2] Notes`,
`[3] Calendar`, `[4] History`. Press the number to jump straight to that
tab from anywhere (any tab, any panel) while in Normal mode. `Tab` /
`Shift+Tab` still cycle if you prefer. Numbers are only intercepted in
Normal mode, so they never fire while you're typing a note name, a todo,
a search, or editing a note body.

**Global note bookmarks.** Vim-style marks for notes that work across the
whole app and sync between devices:

- **Set:** with a note selected in the Notes list, press `g` then `m`
  then a key (any letter or digit) — e.g. `gmw` bookmarks the previewed
  note under `w`.
- **Jump:** press `g` then that key — e.g. `gw` — from *any* tab to open
  the bookmarked note in the editor. It also drops you into that note's
  folder, so closing the editor lands you back there in context.
- The status bar prompts you after `g`, and `Esc` cancels the chord.
  `m` is reserved as the set-prefix, so it can't be a bookmark key
  itself; every other alphanumeric can. A bookmark pointing at a
  since-deleted note says so rather than doing nothing.

Bookmarks live in the synced document, so they persist across restarts
and appear on every device — like Vim's uppercase global marks, but
shared.

## Todos & tags

The Todos tab (`[2]`) is an ordered, reorderable list — `a` add, `e` /
`Enter` edit, `d` delete, `J` / `K` move a todo up/down. On top of that,
todos can be **tagged** and the list **filtered** to a tag:

**Tag todos.**

- Press `Space` on a todo to multi-select it (a `◉` marks selected rows);
  press `Space` again to deselect. Select as many as you like.
- Press `t` to tag them. If nothing is multi-selected, `t` tags just the
  todo under the cursor.
- Type a tag name and press `Enter`. While typing, `Tab` autocompletes
  against tags you already use — press it repeatedly to cycle the
  candidates that match what you've typed so far.
- A todo can carry any number of tags; tags show as magenta `#chips` on
  the row. Tagging is additive and idempotent — re-tagging does nothing.

**Filter to a tag.**

- Press `f`, type a tag (with the same `Tab` autocompletion), `Enter`.
  The list collapses to just the todos carrying that tag.
- The active filter is shown prominently in the list header
  (`▶ #tag ◀`). While a filter is on, **any new todo you add with `a`
  is automatically given that tag**, so it stays visible instead of
  silently dropping out of the filtered view.
- Clear the filter by pressing `Esc` (or `f` then an empty `Enter`).

Tags are part of the synced document, so they converge across devices and
are captured by checkpoints / history exactly like note content — restoring
to an earlier point restores the tags as they were then.

## Sync
Automerge is both the storage format and the sync protocol — it's designed for offline-first distributed synchronisation. `ccal-server` is a small always-on **peer** you run on your own machine: clients all point at it, it merges everything server-side, rebroadcasts so other connected clients converge live, and acts as a free plaintext backup of the whole corpus. No database, no schema, no migrations. It's intended to run behind Tailscale, so the server itself doesn't do transport encryption — you let Tailscale do that — though a bearer token is still checked at the handshake regardless.

### Config file

Both the `ccal` TUI and `ccal-server` read an optional TOML config file. See [`config.example.toml`](config.example.toml) for every setting with its default in a comment — copy it and uncomment what you need.

ccal looks for the file in this order:

1. `$CCAL_CONFIG`, if set (any path you like)
2. otherwise the OS config directory:
   - **Linux:** `~/.config/ccal/config.toml` (or `$XDG_CONFIG_HOME/ccal/config.toml` if `XDG_CONFIG_HOME` is set)
   - **macOS:** `~/Library/Application Support/ccal/config.toml`

A missing file is fine — it just means env vars / defaults only. Precedence for every value is **environment variable > config file > built-in default**, so existing `CCAL_*` env-based deployments keep working unchanged.

Minimal client setup (point the TUI at your server):

```toml
token = "a-long-random-string"

[client]
url = "ws://your-server:8787/sync/ccal"
```

Server listening on all interfaces on a custom port:

```toml
token = "a-long-random-string"

[server]
addr = "0.0.0.0:9000"
```

## MCP server (optional)

`ccal-server` can expose an [MCP](https://modelcontextprotocol.io) server so
an LLM coding assistant can list, read, create, edit, move, rename and
delete your notes and todos — i.e. apply its intelligence to organising and
summarising the corpus. It's **off by default** (the surface is full
read+write) and served at `/mcp` on the *same* address and behind the *same*
bearer token as sync:

```toml
token = "a-long-random-string"

[server]
addr = "0.0.0.0:8787"
mcp = true            # or env CCAL_MCP=1
```

Because it edits the live document and reuses the sync change-broadcast,
anything the assistant changes shows up **immediately** in every connected
ccal client. Point an assistant at it over HTTP:

```
claude mcp add --transport http ccal http://your-server:8787/mcp \
  --header "Authorization: Bearer a-long-random-string"
```

`mcp_doc` (default `ccal`, env `CCAL_MCP_DOC`) picks which docid it edits —
keep it equal to the docid your clients sync (the last path segment of the
client `url`).

## Checkpoints, history & privacy

These work whether or not you use the MCP server, but they're what makes
letting an assistant loose on your notes safe.

**Checkpoints / undo.** Because Automerge keeps the whole history anyway, a
checkpoint is basically free — just a label plus the document heads at that
moment. The assistant is instructed to `create_checkpoint` (with a reason)
before and after any batch of changes; if it makes a mess you (or it) can
`preview_restore` to see exactly what would change and then
`restore_checkpoint`. Restore isn't a history rewind (a CRDT can't do
that) — it's a normal forward edit that snaps the **whole** corpus back to
that point, so it syncs to every client like any other change. Note the
"whole corpus": restoring also reverts unrelated edits made after the
checkpoint, which is fine for a single user but worth knowing.

**Private notes.** Press `p` on a note (normal mode) to mark it private —
it shows a 🔒 in the tree. The assistant can no longer read or edit its
body (`get_note` returns "content redacted", `update_note_body` is
refused), but it can still rename, move or delete it, so it can keep
organising your folders without ever seeing passwords or other sensitive
content. This is enforcement *against the LLM*, not encryption — the
content is still plain in the store and syncs to your own devices normally,
and there is deliberately no way for the assistant to un-private a note.
One caveat worth knowing: privacy isn't retroactive. It protects the note
from the moment you set it; a checkpoint or history from *before* you
marked it private still holds whatever was in it then, and restoring to
that point brings that older content back (un-private). So mark a note
private *before* you put secrets in it — the app reminds you of this when
you toggle it.

**Search.** Press `/` in the notes view to search across every note
(title, folder path and body). The folder tree collapses to a live list of
matches as you type; ↑↓ to pick, Enter to open, Esc to cancel. The
assistant has the same thing as a `search_notes` tool — with one
difference: a private note can only be matched by its title or folder, never
its hidden body, so search can't be used to fish for secrets.

**History & time travel.** A third tab (`Tab` to cycle to it) shows the
full edit timeline, newest first — every change, with named snapshots
marked ★ inline. `c` creates a named snapshot (a checkpoint with a reason);
`p` previews exactly what restoring a point would change; `r` restores —
and you can roll back to *any* change in the log, not just named ones, not
just the assistant's. Restoring isn't a history rewind (Automerge can't do
that): it's a normal forward edit that snaps the whole corpus back to that
point, so it syncs to every device like any other change. Because the
assistant is told to checkpoint before/after each batch, there's always a
labelled point to come back to if it makes a mess.



## Web app

`ccal-server` can also serve a **web app** — the same notes & todos as the
TUI, in the browser, phone-first. It's a true Automerge **peer**: the whole
document runs client-side over the same `/sync` socket, so editing is local,
conflict-free and **offline-capable** — the doc is cached in IndexedDB, so a
cold, offline launch shows your last synced state and queues edits until the
socket returns. It's an installable PWA ("Add to Home Screen" for an app-like
window), and on a phone the notes and todos become tabs.

It's **embedded in the release `ccal-server` binary** — run the server and
open its address in a browser; there's no separate web host. Enter your bearer
token once (browsers can't send an `Authorization` header on a WebSocket, so
the token rides the `Sec-WebSocket-Protocol` subprotocol).

The web client is a fully-trusted peer, equal to the TUI — it receives the
whole document, including private-note bodies, so it's meant for your own
devices behind Tailscale, the same trust model as the server. The private flag
shows as a 🔒 marker but, deliberately, can't be toggled from the web —
privacy stays a TUI action so there's one authority for it. History /
checkpoints are likewise TUI-only.

**Build it** (the release does this automatically):

```sh
cd web && npm ci && npm run build      # produces web/dist
cargo build --release --features web   # ccal-server embeds web/dist
```

For development, run `ccal-server` normally and use the vite dev server (hot
reload), which proxies `/sync` to it:

```sh
cd web && npm run dev                  # http://localhost:5173
# non-default server: CCAL_SERVER=host:port npm run dev
```

Setting `CCAL_WEB_DIR=/path/to/web/dist` makes a `--features web` server serve
assets from disk instead of the embedded copy — handy for iterating without a
rebuild.

## Mobile App
I want a very basic mobile app, as I think up ideas in the night or out and about, and I need a way to capture them.  It will never be fully featured — just a way to get notes/todos into this system and view them.  Decision made: it's a Dioxus app (iOS first), reusing the Rust core and sync, and it's Tailscale-dependent like the server.  The full design is in [docs/plans/mobile-app.md](docs/plans/mobile-app.md).

## License

GPLv3. See [LICENSE](LICENSE).
