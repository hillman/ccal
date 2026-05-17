CCAL Reborn

Way back in the day, when I was at university, myself and a friend made a curses based calendar/todo list app called Ccal.  We loved it and it had a niche community of people who used it, it was fun.  But work got in the way and it died.  I thought, in this age of LLM assisted side projects, where you can get much more done with the limited time you have, that I'd have a go at ressurecting it in Rust.  It's a spiritual successor, not a re-build. 

It's for notes, todos and (not implemented yet) calendar stuff.  Despite the name, the calendar bit is actually secondary and will really just be about pulling in today's events from other calendars and having timed todos.  It isn't a fully fledged calendar.

I've built in Rust for nice quick UI and easy executable builds.  It's using automerge as a CRDT/protocol for storage, so I can add a simple sync server (not done yet).


