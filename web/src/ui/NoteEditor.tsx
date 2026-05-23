import { useState } from "react";
import type { Store } from "../store";
import type { NoteView } from "../schema";

// Mounted with key={note.id} (see App), so local state re-initialises when the
// selected note changes. Edits push live (title/body/folder); on mobile the
// editor is full-screen with a Save button that commits-and-returns (edits are
// already synced, so Save = done). A remote edit to the *currently open* note
// won't refresh the textarea until reselect — the documented v1 limitation;
// CodeMirror + automerge-codemirror is the upgrade.
export function NoteEditor({
  store,
  note,
  onDeleted,
  onBack,
  mobile = false,
}: {
  store: Store;
  note: NoteView;
  onDeleted: () => void;
  onBack?: () => void;
  mobile?: boolean;
}) {
  const [title, setTitle] = useState(note.title);
  const [body, setBody] = useState(note.body);
  const [folderStr, setFolderStr] = useState(note.folder.join("/"));

  const del = () => {
    if (confirm("Delete this note?")) {
      store.deleteNote(note.id);
      onDeleted();
    }
  };
  const lock = note.private ? (
    <span className="lock" title="private — toggle in the TUI">
      🔒
    </span>
  ) : null;

  return (
    <div className={`editor${mobile ? " editor-mobile" : ""}`}>
      {mobile && (
        <div className="editor-bar">
          <button className="primary" onClick={onBack}>
            ‹ Save
          </button>
          <span className="spacer" />
          {lock}
          <button className="danger" onClick={del}>
            delete
          </button>
        </div>
      )}

      <div className="editor-head">
        <input
          className="title"
          value={title}
          placeholder="Title"
          onChange={(e) => {
            setTitle(e.target.value);
            store.setNoteTitle(note.id, e.target.value);
          }}
        />
        {!mobile && lock}
        {!mobile && (
          <button className="danger" onClick={del}>
            delete
          </button>
        )}
      </div>

      <input
        className="folder"
        value={folderStr}
        placeholder="folder/path"
        onChange={(e) => setFolderStr(e.target.value)}
        onBlur={() => store.moveNote(note.id, parseFolder(folderStr))}
      />

      <textarea
        className="body"
        value={body}
        placeholder="Write…"
        onChange={(e) => {
          const next = e.target.value;
          store.editNoteBody(note.id, body, next);
          setBody(next);
        }}
      />

      {!mobile && <div className="meta">edited {new Date(note.modified).toLocaleString()}</div>}
    </div>
  );
}

function parseFolder(s: string): string[] {
  return s
    .split("/")
    .map((x) => x.trim())
    .filter(Boolean);
}
