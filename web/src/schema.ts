// The cross-implementation document contract, TS side. Authority is
// src/store.rs's module doc-comment; this mirrors it and is kept honest by
// test/conformance.ts. We use the CLASSIC @automerge/automerge API (default
// export) because it maps 1:1 to the Rust schema (verified by the conformance
// harness):
//   plain string -> Str (scalar)   |   new Automerge.Text -> Text (CRDT)
//   new Automerge.Int -> Int       |   array -> List   |   number -> F64
// The doc also contains checkpoints/cal/mark; the web client ignores them.
//
// Mutation drafts are typed `any`: the Automerge change-proxy accepts wrapper
// values (Int/Text) that don't match the plain read-side field types, and
// fighting that in the type system buys nothing. Reads stay fully typed.

import * as Automerge from "@automerge/automerge";

export type Doc = Automerge.Doc<CcalDoc>;

export const SCHEMA_VERSION = 1;

export interface CcalDoc {
  schema: number;
  notes: { [id: string]: NoteFields };
  todos: { [id: string]: TodoFields };
}
export interface NoteFields {
  title: string;
  folder: string[];
  body: Automerge.Text | string;
  created: number;
  modified: number;
  private?: boolean;
}
export interface TodoFields {
  text: string;
  order: number;
  created: number;
}

// ---- Read-side projections (derived, never stored) --------------------

export interface NoteView {
  id: string;
  title: string;
  folder: string[];
  body: string;
  created: number;
  modified: number;
  private: boolean;
}
export interface TodoView {
  id: string;
  text: string;
  order: number;
  created: number;
}

/** True once the server's genesis has populated the doc. The client must
 *  NEVER seed these maps itself — that is the genesis hazard called out in
 *  store.rs. Render "syncing…" until this holds. */
export function isAdopted(doc: Doc): boolean {
  const d = doc as unknown as Partial<CcalDoc>;
  return d.schema === SCHEMA_VERSION && !!d.notes && !!d.todos;
}

function bodyToString(b: Automerge.Text | string | undefined): string {
  if (b == null) return "";
  return typeof b === "string" ? b : b.toString();
}

export function readNotes(doc: Doc): NoteView[] {
  const notes = (doc as unknown as CcalDoc).notes ?? {};
  return Object.entries(notes).map(([id, n]) => ({
    id,
    title: n.title ?? "",
    folder: n.folder ? Array.from(n.folder) : [],
    body: bodyToString(n.body),
    created: Number(n.created ?? 0),
    modified: Number(n.modified ?? 0),
    private: !!n.private,
  }));
}

export function readTodos(doc: Doc): TodoView[] {
  const todos = (doc as unknown as CcalDoc).todos ?? {};
  return Object.entries(todos)
    .map(([id, t]) => ({
      id,
      text: t.text ?? "",
      order: Number(t.order ?? 0),
      created: Number(t.created ?? 0),
    }))
    .sort((a, b) => a.order - b.order || a.created - b.created);
}

// ---- Folder tree derivation (folders are derived; an empty folder simply
//      ceases to exist — mirrors store.rs) ---------------------------------

export interface FolderNode {
  name: string;
  path: string[];
  folders: FolderNode[];
  notes: NoteView[];
}

export function folderTree(notes: NoteView[]): FolderNode {
  const root: FolderNode = { name: "", path: [], folders: [], notes: [] };
  const ensure = (path: string[]): FolderNode => {
    let cur = root;
    const acc: string[] = [];
    for (const comp of path) {
      acc.push(comp);
      let next = cur.folders.find((f) => f.name === comp);
      if (!next) {
        next = { name: comp, path: [...acc], folders: [], notes: [] };
        cur.folders.push(next);
      }
      cur = next;
    }
    return cur;
  };
  for (const n of notes) ensure(n.folder).notes.push(n);
  const sortRec = (f: FolderNode) => {
    f.folders.sort((a, b) => a.name.localeCompare(b.name));
    // Most-recently-modified first; id breaks ties for a stable order.
    f.notes.sort((a, b) => b.modified - a.modified || a.id.localeCompare(b.id));
    f.folders.forEach(sortRec);
  };
  sortRec(root);
  return root;
}

// ---- Write-side helpers (mirror store.rs mutations) -------------------
// Each returns a NEW doc (Automerge is immutable). They assume the genesis
// maps exist (guard with isAdopted) and NEVER create them.

const now = () => new Automerge.Int(Date.now());

// Automerge.Text.insertAt takes chars as varargs; chunk large inserts so a
// big paste can't blow the call-stack / argument limit.
function insertChars(body: Automerge.Text, pos: number, chars: string[]) {
  const CHUNK = 1000;
  for (let i = 0; i < chars.length; i += CHUNK) {
    body.insertAt(pos + i, ...chars.slice(i, i + CHUNK));
  }
}

export function createNote(doc: Doc, folder: string[], title: string): [Doc, string] {
  const id = crypto.randomUUID();
  const ts = now();
  const next = Automerge.change(doc, (d: any) => {
    d.notes[id] = {
      title,
      folder: [...folder],
      body: new Automerge.Text(""),
      created: ts,
      modified: ts,
    };
  });
  return [next, id];
}

export function setNoteTitle(doc: Doc, id: string, title: string): Doc {
  return Automerge.change(doc, (d: any) => {
    const n = d.notes[id];
    if (!n) return;
    n.title = title;
    n.modified = now();
  });
}

export function moveNote(doc: Doc, id: string, folder: string[]): Doc {
  return Automerge.change(doc, (d: any) => {
    const n = d.notes[id];
    if (!n) return;
    n.folder = [...folder];
    n.modified = now();
  });
}

export function deleteNote(doc: Doc, id: string): Doc {
  return Automerge.change(doc, (d: any) => {
    delete d.notes[id];
  });
}

/** Apply just the user's local delta (`prev` -> `next`) to the body Text, so
 *  a concurrent remote edit to the same note is preserved rather than
 *  clobbered. This is what the editor calls on each keystroke; `setNoteBody`
 *  (full reconcile against the doc) is for programmatic/whole-value sets. */
export function editNoteBody(doc: Doc, id: string, prev: string, next: string): Doc {
  return Automerge.change(doc, (d: any) => {
    const n = d.notes[id];
    if (!n) return;
    const body = n.body as Automerge.Text;
    const diff = textSplice(prev, next);
    if (!diff) return;
    const [pos, del, ins] = diff;
    if (del > 0) body.deleteAt(pos, del);
    if (ins.length > 0) insertChars(body, pos, ins);
    n.modified = now();
  });
}

/** Reconcile the whole body to `next` by splicing only the changed region
 *  against the doc's current text — mirrors store.rs `set_note_body`. */
export function setNoteBody(doc: Doc, id: string, next: string): Doc {
  return Automerge.change(doc, (d: any) => {
    const n = d.notes[id];
    if (!n) return;
    const body = n.body as Automerge.Text;
    const diff = textSplice(body.toString(), next);
    if (!diff) return;
    const [pos, del, ins] = diff;
    if (del > 0) body.deleteAt(pos, del);
    if (ins.length > 0) insertChars(body, pos, ins);
    n.modified = now();
  });
}

export function addTodo(doc: Doc, text: string): [Doc, string] {
  const id = crypto.randomUUID();
  const orders = readTodos(doc).map((t) => t.order);
  const order = orders.length ? Math.max(...orders) + 1 : 1; // mirror add_todo
  const next = Automerge.change(doc, (d: any) => {
    d.todos[id] = { text, order, created: now() };
  });
  return [next, id];
}

export function setTodoText(doc: Doc, id: string, text: string): Doc {
  return Automerge.change(doc, (d: any) => {
    if (d.todos[id]) d.todos[id].text = text;
  });
}

export function deleteTodo(doc: Doc, id: string): Doc {
  return Automerge.change(doc, (d: any) => {
    delete d.todos[id];
  });
}

/** Move `id` to position `toIndex` among the other todos by choosing a
 *  fractional order key between its new neighbours — never a full renumber. */
export function reorderTodo(doc: Doc, id: string, toIndex: number): Doc {
  const others = readTodos(doc).filter((t) => t.id !== id);
  const before = others[toIndex - 1]?.order;
  const after = others[toIndex]?.order;
  let order: number;
  if (before == null && after == null) order = 1;
  else if (before == null) order = after! - 1;
  else if (after == null) order = before + 1;
  else order = (before + after) / 2;
  return Automerge.change(doc, (d: any) => {
    if (d.todos[id]) d.todos[id].order = order;
  });
}

/** Minimal (prefix-position, delete-count, insert-chars) edit turning `cur`
 *  into `next`, or null if unchanged. Code-point based, like Rust
 *  `text_splice`. */
export function textSplice(cur: string, next: string): [number, number, string[]] | null {
  if (cur === next) return null;
  const a = [...cur];
  const b = [...next];
  const max = Math.min(a.length, b.length);
  let p = 0;
  while (p < max && a[p] === b[p]) p++;
  let s = 0;
  while (s < max - p && a[a.length - 1 - s] === b[b.length - 1 - s]) s++;
  return [p, a.length - p - s, b.slice(p, b.length - s)];
}
