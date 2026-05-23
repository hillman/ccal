import { useState } from "react";
import type { Store } from "../store";
import type { NoteView } from "../schema";

// Mounted with key={note.id} (see App), so local state re-initialises when
// the selected note changes. While a note is open the textarea is the local
// source of truth and edits are pushed as minimal splices (store.editNoteBody
// diffs prev->next), so concurrent remote edits are preserved. A remote edit
// to the *currently open* note won't refresh the textarea until reselect —
// the documented v1 limitation; CodeMirror + automerge-codemirror is the
// upgrade (see docs/plans/web-interface.md, P3).
export function NoteEditor({
  store,
  note,
  onDeleted,
}: {
  store: Store;
  note: NoteView;
  onDeleted: () => void;
}) {
  const [title, setTitle] = useState(note.title);
  const [body, setBody] = useState(note.body);
  const [folderStr, setFolderStr] = useState(note.folder.join("/"));

  return (
    <div className="editor">
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
        {note.private && (
          <span className="lock" title="private — toggle in the TUI">
            🔒
          </span>
        )}
        <button
          className="danger"
          onClick={() => {
            if (confirm("Delete this note?")) {
              store.deleteNote(note.id);
              onDeleted();
            }
          }}
        >
          delete
        </button>
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

      <div className="meta">edited {new Date(note.modified).toLocaleString()}</div>
    </div>
  );
}

function parseFolder(s: string): string[] {
  return s
    .split("/")
    .map((x) => x.trim())
    .filter(Boolean);
}
