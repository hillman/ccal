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
- **Folders are derived, not managed.** A note's folder is just its path —
  create, move and rename folders implicitly by filing notes; folder rename
  rewrites a whole subtree at once. No empty-folder bookkeeping.
- **Vim note editor.** Modal editing via [edtui](https://github.com/preiter93/edtui)
  with markdown syntax highlighting; app keys only fire in Normal mode, so
  typing never triggers a command.
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



## Mobile App
I want a very basic mobile app, as I think up ideas in the night or out and about, and I need a way to capture them.  It will never be fully featured — just a way to get notes/todos into this system and view them.  Decision made: it's a Dioxus app (iOS first), reusing the Rust core and sync, and it's Tailscale-dependent like the server.  The full design is in [docs/plans/mobile-app.md](docs/plans/mobile-app.md).

## License

GPLv3. See [LICENSE](LICENSE).
