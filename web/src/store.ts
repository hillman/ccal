// The reactive store: the single local Automerge doc plus its sync
// connection. React subscribes via `useSyncExternalStore`; the UI never holds
// doc state of its own, it renders the projection recomputed on every applied
// change (local or remote). Mutations go straight to the local doc and are
// pushed to the peer.

import * as Automerge from "@automerge/automerge";
import { connect, type SyncConn, type ConnStatus } from "./sync";
import type { Doc, NoteView, TodoView } from "./schema";
import * as S from "./schema";
import { loadDoc, saveDoc, deleteDoc } from "./storage";
import { DOC_ID } from "./config";

export interface Snapshot {
  status: ConnStatus;
  adopted: boolean;
  notes: NoteView[];
  todos: TodoView[];
}

const EMPTY: Snapshot = { status: "connecting", adopted: false, notes: [], todos: [] };

export class Store {
  // Blank doc; it ADOPTS the genesis via sync (or from IndexedDB on boot) and
  // must never seed it itself.
  private doc: Doc = Automerge.init();
  private conn: SyncConn | null = null;
  private status: ConnStatus = "connecting";
  private listeners = new Set<() => void>();
  private snap: Snapshot = EMPTY;
  private saveTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(url: string, token: string) {
    this.boot(url, token);
  }

  // Load the cached doc first (so an offline cold-start shows data), then
  // connect. Merging — not overwriting — keeps any changes a fast sync may
  // have already applied to the blank doc.
  private async boot(url: string, token: string): Promise<void> {
    try {
      const bytes = await loadDoc(DOC_ID);
      if (bytes) {
        const cached = Automerge.load(bytes) as Doc;
        // Discard a pre-line-body (schema < 2) cache instead of merging it:
        // syncing its per-character history would re-bloat the migrated
        // server. Drop it and re-pull the small new doc fresh.
        if (S.schemaOf(cached) >= S.SCHEMA_VERSION) {
          this.doc = Automerge.merge(this.doc, cached);
          this.recompute();
        } else {
          await deleteDoc(DOC_ID);
        }
      }
    } catch {
      /* a corrupt/absent cache is not fatal — sync will repopulate */
    }
    this.conn = connect(url, token, {
      getDoc: () => this.doc,
      setDoc: (d) => {
        this.doc = d;
        this.scheduleSave();
        this.recompute();
      },
      onStatus: (s) => {
        this.status = s;
        this.recompute();
      },
    });
  }

  // Debounced persist — the client-side mirror of the server's saver.
  private scheduleSave(): void {
    if (this.saveTimer) clearTimeout(this.saveTimer);
    this.saveTimer = setTimeout(() => {
      saveDoc(DOC_ID, Automerge.save(this.doc)).catch(() => {});
    }, 1000);
  }

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  // Stable reference between changes — required by useSyncExternalStore.
  getSnapshot = (): Snapshot => this.snap;

  private recompute(): void {
    const adopted = S.isAdopted(this.doc);
    this.snap = {
      status: this.status,
      adopted,
      notes: adopted ? S.readNotes(this.doc) : [],
      todos: adopted ? S.readTodos(this.doc) : [],
    };
    this.listeners.forEach((l) => l());
  }

  private apply(d: Doc): void {
    this.doc = d;
    this.conn?.push(); // may be null briefly during boot; edits still persist
    this.scheduleSave();
    this.recompute();
  }

  // ---- note mutations ----
  createNote(folder: string[], title: string): string {
    const [d, id] = S.createNote(this.doc, folder, title);
    this.apply(d);
    return id;
  }
  setNoteTitle(id: string, title: string): void {
    this.apply(S.setNoteTitle(this.doc, id, title));
  }
  editNoteBody(id: string, prev: string, next: string): void {
    this.apply(S.editNoteBody(this.doc, id, prev, next));
  }
  moveNote(id: string, folder: string[]): void {
    this.apply(S.moveNote(this.doc, id, folder));
  }
  deleteNote(id: string): void {
    this.apply(S.deleteNote(this.doc, id));
  }

  // ---- todo mutations ----
  addTodo(text: string): string {
    const [d, id] = S.addTodo(this.doc, text);
    this.apply(d);
    return id;
  }
  setTodoText(id: string, text: string): void {
    this.apply(S.setTodoText(this.doc, id, text));
  }
  deleteTodo(id: string): void {
    this.apply(S.deleteTodo(this.doc, id));
  }
  reorderTodo(id: string, toIndex: number): void {
    this.apply(S.reorderTodo(this.doc, id, toIndex));
  }

  close(): void {
    this.conn?.close();
  }
}
