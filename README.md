# CCAL Reborn

A terminal UI for notes, todo and some basic calendar stuff.  Also has a sync server based on autosync.

## Why?

Way back in the day, when I was at university, myself and a friend made a curses based calendar/todo list app called Ccal.  We loved it and it had a niche community of people who used it, it was fun.  But work got in the way and it died.  I thought, in this age of LLM assisted side projects, where you can get much more done with the limited time you have, that I'd have a go at ressurecting it in Rust.  It's a spiritual successor, not a re-build. 

It's for notes, todos and (not implemented yet) calendar stuff.  Despite the name, the calendar bit is actually secondary and will really just be about pulling in today's events from other calendars and having timed todos.  It isn't a fully fledged calendar.

I've built in Rust for nice quick UI and easy executable builds.  It's using automerge as a CRDT/protocol for storage, so I can add a simple sync server (not done yet).

## Editor

It's using edtui for the note editor - that uses vim bindings, because I love vim and have it in my fingers.  Sorry, it's an opionated choice.  It'd be easy enough to implement a config flag for a simpler edit component though if anyone wants it. PR welcome.


## Sync
Automerge is the storage format and the sync protocol - it's designed for offline first distributed synchronisation.  There will be a sync peer you can run on your own machine and that can be used to sync clients - they all just point to that.  This is intended to be used behind tailscale, so the server doesn't implement encryption or auth, you just let tailscale do that.

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

### MCP server (optional)

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



## Mobile App
I want a very basic mobile app, as I think up ideas in the night or out and about, and I need a way to capture them.  It will never be fully featured — just a way to get notes/todos into this system and view them.  Decision made: it's a Dioxus app (iOS first), reusing the Rust core and sync, and it's Tailscale-dependent like the server.  The full design is in [docs/plans/mobile-app.md](docs/plans/mobile-app.md).

## License

GPLv3. See [LICENSE](LICENSE).
