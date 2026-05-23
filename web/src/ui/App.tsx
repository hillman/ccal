import { useEffect, useMemo, useState, useSyncExternalStore } from "react";
import { Store } from "../store";
import { getToken, setToken, clearToken, syncUrl } from "../config";
import { FolderTree } from "./FolderTree";
import { NoteEditor } from "./NoteEditor";
import { TodoList } from "./TodoList";
import { folderTree } from "../schema";
import { useIsMobile } from "./useIsMobile";

export function App() {
  const [token, setTok] = useState<string | null>(getToken());
  if (!token) return <TokenGate onToken={(t) => { setToken(t); setTok(t); }} />;
  return <Workspace token={token} onLogout={() => { clearToken(); setTok(null); }} />;
}

function TokenGate({ onToken }: { onToken: (t: string) => void }) {
  const [value, setValue] = useState("");
  return (
    <form
      className="gate"
      onSubmit={(e) => {
        e.preventDefault();
        if (value.trim()) onToken(value.trim());
      }}
    >
      <h1>ccal</h1>
      <p>Enter the sync token to connect.</p>
      <input
        type="password"
        autoFocus
        value={value}
        placeholder="bearer token"
        onChange={(e) => setValue(e.target.value)}
      />
      <button type="submit">Connect</button>
    </form>
  );
}

function Workspace({ token, onLogout }: { token: string; onLogout: () => void }) {
  // One store per token, for the lifetime of this view.
  const store = useMemo(() => new Store(syncUrl(), token), [token]);
  useEffect(() => () => store.close(), [store]);
  const snap = useSyncExternalStore(store.subscribe, store.getSnapshot);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [folder, setFolder] = useState<string[]>([]);
  // Which pane is shown on narrow screens. Ignored on wide screens (CSS shows
  // both); the tab bar itself is hidden there too.
  const [tab, setTab] = useState<"notes" | "todos">("notes");

  const tree = useMemo(() => folderTree(snap.notes), [snap.notes]);
  const selected = snap.notes.find((n) => n.id === selectedId) ?? null;
  const isMobile = useIsMobile();

  // Mobile: editing a note takes over the whole screen (its own back/save
  // bar; no app chrome) so there's room to write on a phone.
  if (isMobile && snap.adopted && tab === "notes" && selected) {
    return (
      <div className="app">
        <NoteEditor
          key={selected.id}
          store={store}
          note={selected}
          mobile
          onBack={() => setSelectedId(null)}
          onDeleted={() => setSelectedId(null)}
        />
      </div>
    );
  }

  return (
    <div className="app">
      <header className="bar">
        <strong>ccal</strong>
        <span className={`status status-${snap.status}`}>{snap.status}</span>
        <span className="spacer" />
        <button className="link" onClick={onLogout}>change token</button>
      </header>

      {!snap.adopted ? (
        <div className="syncing">
          {snap.status === "online" ? "syncing…" : `waiting for server (${snap.status})…`}
        </div>
      ) : (
        <>
        <nav className="tabs">
          <button className={tab === "notes" ? "active" : ""} onClick={() => setTab("notes")}>
            Notes
          </button>
          <button className={tab === "todos" ? "active" : ""} onClick={() => setTab("todos")}>
            Todos
          </button>
        </nav>
        <div className="panes" data-tab={tab}>
          <aside className="sidebar">
            <div className="sidebar-actions">
              <span className="crumb">/{folder.join("/")}</span>
              <button
                onClick={() => {
                  const id = store.createNote(folder, "Untitled");
                  setSelectedId(id);
                }}
              >
                + note
              </button>
            </div>
            <FolderTree
              root={tree}
              selectedId={selectedId}
              activeFolder={folder}
              onSelectNote={setSelectedId}
              onSelectFolder={setFolder}
            />
          </aside>

          <main className="editor-pane">
            {selected ? (
              <NoteEditor key={selected.id} store={store} note={selected} onDeleted={() => setSelectedId(null)} />
            ) : (
              <div className="empty">Select or create a note.</div>
            )}
          </main>

          <aside className="todos-pane">
            <TodoList store={store} todos={snap.todos} />
          </aside>
        </div>
        </>
      )}
    </div>
  );
}
