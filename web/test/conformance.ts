// P1 conformance — JS side. Drives the real ccal-server with the schema.ts
// mutation helpers, then re-reads through a second fresh peer and asserts the
// projections. This proves the TS write helpers + readers + folder-tree +
// fractional reorder all round-trip through the actual sync protocol. The
// Rust half (tests/conformance.rs) then asserts the same doc is readable by
// `Store`, so the two implementations cannot silently drift.
//
// Run via the orchestrator (web/test/run-conformance.sh) against a running
// server. Auth uses the `ccal.bearer.<token>` subprotocol (Node 22 has a
// global WebSocket), exercising the same browser auth path as production.

import * as Automerge from "@automerge/automerge";
import * as S from "../src/schema.ts";
import type { Doc } from "../src/schema.ts";

const SERVER = process.env.CCAL_CONF_SERVER ?? "ws://127.0.0.1:8799";
const DOCID = process.env.CCAL_CONF_DOC ?? "ccal";
const TOKEN = process.env.CCAL_SYNC_TOKEN;

if (!TOKEN) {
  console.error("conformance: set CCAL_SYNC_TOKEN");
  process.exit(2);
}

const die = (m: string): never => {
  console.error(`conformance: FAIL — ${m}`);
  process.exit(1);
};
const assert = (cond: unknown, m: string) => {
  if (!cond) die(m);
};

// Connect, drive the raw sync loop to an idle gap, and return the converged
// doc plus a `mutate` that applies a change and flushes it.
function open(): Promise<{
  getDoc: () => Doc;
  mutate: (f: (d: Doc) => Doc) => void;
  done: () => Promise<void>;
}> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`${SERVER}/sync/${DOCID}`, [`ccal.bearer.${TOKEN}`]);
    ws.binaryType = "arraybuffer";
    let doc: Doc = Automerge.init();
    let sync = Automerge.initSyncState();
    let idle: ReturnType<typeof setTimeout> | null = null;
    let onIdle: () => void = () => {};

    const flush = () => {
      for (;;) {
        const [next, msg] = Automerge.generateSyncMessage(doc, sync);
        sync = next;
        if (!msg) break;
        ws.send(msg);
      }
    };
    const arm = () => {
      if (idle) clearTimeout(idle);
      idle = setTimeout(() => onIdle(), 600);
    };

    ws.onerror = () => reject(new Error("websocket error"));
    ws.onopen = () => {
      flush();
      arm();
    };
    ws.onmessage = (ev) => {
      [doc, sync] = Automerge.receiveSyncMessage(doc, sync, new Uint8Array(ev.data as ArrayBuffer));
      flush();
      arm();
    };

    // First idle = initial convergence done.
    onIdle = () =>
      resolve({
        getDoc: () => doc,
        mutate: (f) => {
          doc = f(doc);
          flush();
        },
        // Resolve once a post-mutation idle gap is reached, then close.
        done: () =>
          new Promise<void>((res) => {
            onIdle = () => {
              ws.close();
              res();
            };
            arm();
          }),
      });
  });
}

async function main() {
  // --- write side: mutate via schema.ts helpers --------------------------
  const w = await open();
  assert(S.isAdopted(w.getDoc()), "writer did not adopt genesis");

  let noteId = "";
  w.mutate((d) => {
    const [d2, id] = S.createNote(d, ["work", "proj"], "P1 note");
    noteId = id;
    return d2;
  });
  // Multi-line so the body `List<Str>` representation is exercised (not just
  // a single-element list).
  w.mutate((d) => S.editNoteBody(d, noteId, "", "alpha\nbravo"));
  w.mutate((d) => S.setNoteTitle(d, noteId, "P1 note edited"));

  const todoIds: string[] = [];
  for (const t of ["first", "second", "third"]) {
    w.mutate((d) => {
      const [d2, id] = S.addTodo(d, t);
      todoIds.push(id);
      return d2;
    });
  }
  // Move "third" (index 2) to the front (index 0).
  w.mutate((d) => S.reorderTodo(d, todoIds[2], 0));
  await w.done();

  // give the server's debounced saver time to persist for the Rust readback
  await new Promise((r) => setTimeout(r, 2500));

  // --- read side: a fresh peer must see the same projection --------------
  const r = await open();
  const notes = S.readNotes(r.getDoc());
  const note = notes.find((n) => n.id === noteId) ?? die("note not synced to reader");
  assert(note.title === "P1 note edited", `title mismatch: ${note.title}`);
  assert(note.body === "alpha\nbravo", `body mismatch: ${JSON.stringify(note.body)}`);
  assert(note.folder.join("/") === "work/proj", `folder mismatch: ${note.folder}`);

  const tree = S.folderTree(notes);
  const work = tree.folders.find((f) => f.name === "work");
  const proj = work?.folders.find((f) => f.name === "proj");
  assert(proj?.notes.some((n) => n.id === noteId), "folder tree did not place the note under work/proj");

  const todos = S.readTodos(r.getDoc());
  assert(
    todos.map((t) => t.text).join(",") === "third,first,second",
    `todo order mismatch: ${todos.map((t) => t.text).join(",")}`,
  );
  await r.done();

  console.log("conformance: OK — JS write helpers + readers + tree + reorder round-trip through the server");
  process.exit(0);
}

setTimeout(() => die("overall timeout (20s)"), 20_000);
main().catch((e) => die(String(e)));
