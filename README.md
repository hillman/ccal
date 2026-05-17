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



## AI Stuff

I plan to implement a wrapper around rig.rs that lets you hook up to LLMs via openrouter and exposes tools to create, edit, rename and move notes, so I can use LLMs to tidy up and summarise notes.  I will have a flag on a note which says to not expose this particular note to the LLM, and maybe will have an option for default to be LLM doesn't see this, or maybe make it folder level so you can exclude certain folders, or scope a query to folders.  Anyway, just adding this here so that you know, this will  be an LLM-embracing product, but it will always be optional and you'll be able to hook in whatever model you want, probably including local.



## Mobile App
I want a very basic mobile app, as I think up ideas in the night or out and about, and I need a way to capture them.  It will never be fully featured — just a way to get notes/todos into this system and view them.  Decision made: it's a Dioxus app (iOS first), reusing the Rust core and sync, and it's Tailscale-dependent like the server.  The full design is in [docs/plans/mobile-app.md](docs/plans/mobile-app.md).

## License

GPLv3. See [LICENSE](LICENSE).
