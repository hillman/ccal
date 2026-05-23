import { useState } from "react";
import type { FolderNode, NoteView } from "../schema";

interface Common {
  selectedId: string | null;
  activeFolder: string[];
  onSelectNote: (id: string) => void;
  onSelectFolder: (path: string[]) => void;
}

export function FolderTree({ root, ...rest }: { root: FolderNode } & Common) {
  return (
    <div className="tree">
      <NoteList notes={root.notes} selectedId={rest.selectedId} onSelectNote={rest.onSelectNote} />
      {root.folders.map((f) => (
        <Folder key={f.path.join("/")} node={f} {...rest} />
      ))}
    </div>
  );
}

function Folder({ node, ...rest }: { node: FolderNode } & Common) {
  // Keyed by path in the parent, so collapse state survives data re-renders.
  // Collapsed by default; the user expands what they want.
  const [open, setOpen] = useState(false);
  const active = rest.activeFolder.join("/") === node.path.join("/");
  const count = node.notes.length + node.folders.length;
  return (
    <div className="folder">
      <div className={`folder-name${active ? " active" : ""}`}>
        <span
          className="caret"
          onClick={(e) => {
            e.stopPropagation();
            setOpen((o) => !o);
          }}
        >
          {open ? "▾" : "▸"}
        </span>
        <span
          className="folder-label"
          onClick={() => {
            setOpen((o) => !o);
            rest.onSelectFolder(node.path);
          }}
        >
          📁 {node.name}
          {!open && count > 0 && <span className="folder-count"> ({count})</span>}
        </span>
      </div>
      {open && (
        <div className="folder-children">
          <NoteList notes={node.notes} selectedId={rest.selectedId} onSelectNote={rest.onSelectNote} />
          {node.folders.map((f) => (
            <Folder key={f.path.join("/")} node={f} {...rest} />
          ))}
        </div>
      )}
    </div>
  );
}

function NoteList({
  notes,
  selectedId,
  onSelectNote,
}: {
  notes: NoteView[];
  selectedId: string | null;
  onSelectNote: (id: string) => void;
}) {
  return (
    <>
      {notes.map((n) => (
        <div
          key={n.id}
          className={`note-item${n.id === selectedId ? " selected" : ""}`}
          onClick={() => onSelectNote(n.id)}
        >
          {n.private ? "🔒 " : "📄 "}
          {n.title || "Untitled"}
        </div>
      ))}
    </>
  );
}
