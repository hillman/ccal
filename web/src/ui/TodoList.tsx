import { useRef, useState, type FormEvent, type TouchEvent } from "react";
import {
  DndContext,
  closestCenter,
  KeyboardSensor,
  PointerSensor,
  useSensor,
  useSensors,
  type DragEndEvent,
} from "@dnd-kit/core";
import {
  SortableContext,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import type { Store } from "../store";
import type { TodoView } from "../schema";
import { useIsMobile } from "./useIsMobile";

export function TodoList({ store, todos }: { store: Store; todos: TodoView[] }) {
  const [text, setText] = useState("");
  const isMobile = useIsMobile();
  // PointerSensor covers mouse and touch; the handle has touch-action:none so
  // dragging it never scrolls the page. KeyboardSensor keeps reorder a11y.
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  const onDragEnd = ({ active, over }: DragEndEvent) => {
    if (!over || active.id === over.id) return;
    // todos is already in display order; the target index in the full list is
    // exactly what reorderTodo expects (it picks a fractional key there).
    const newIndex = todos.findIndex((t) => t.id === over.id);
    if (newIndex >= 0) store.reorderTodo(String(active.id), newIndex);
  };

  const add = (e: FormEvent) => {
    e.preventDefault();
    const t = text.trim();
    if (t) {
      store.addTodo(t);
      setText("");
    }
  };

  return (
    <div className="todos">
      <h2>Todos</h2>
      <form className="todo-add" onSubmit={add}>
        <input value={text} placeholder="Add a todo…" onChange={(e) => setText(e.target.value)} />
        <button type="submit" className="primary" aria-label="add todo">
          ＋
        </button>
      </form>
      <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={onDragEnd}>
        <SortableContext items={todos.map((t) => t.id)} strategy={verticalListSortingStrategy}>
          <ul className="todo-list">
            {todos.map((t) => (
              <TodoItem
                key={t.id}
                todo={t}
                isMobile={isMobile}
                onText={(v) => store.setTodoText(t.id, v)}
                onDelete={() => store.deleteTodo(t.id)}
              />
            ))}
          </ul>
        </SortableContext>
      </DndContext>
    </div>
  );
}

function TodoItem({
  todo,
  isMobile,
  onText,
  onDelete,
}: {
  todo: TodoView;
  isMobile: boolean;
  onText: (v: string) => void;
  onDelete: () => void;
}) {
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({
    id: todo.id,
  });
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState(todo.text);
  const [dx, setDx] = useState(0); // live swipe offset
  const startX = useRef<number | null>(null);

  const liStyle = {
    transform: CSS.Transform.toString(transform),
    transition,
    zIndex: isDragging ? 2 : undefined,
    opacity: isDragging ? 0.85 : 1,
  };

  // Swipe-right-to-delete: mobile only, and only off the row body (the handle
  // owns reordering, so the two gestures never fight).
  const swipe =
    isMobile && !editing
      ? {
          onTouchStart: (e: TouchEvent) => {
            startX.current = e.touches[0].clientX;
          },
          onTouchMove: (e: TouchEvent) => {
            if (startX.current != null) setDx(Math.max(0, e.touches[0].clientX - startX.current));
          },
          onTouchEnd: () => {
            if (dx > 96) onDelete();
            setDx(0);
            startX.current = null;
          },
        }
      : {};

  return (
    <li ref={setNodeRef} style={liStyle} className="todo-swipe">
      <span className="todo-swipe-hint">🗑</span>
      <div
        className={`todo-row${isDragging ? " dragging" : ""}`}
        style={{ transform: dx ? `translateX(${dx}px)` : undefined }}
      >
        <button className="handle" aria-label="drag to reorder" {...attributes} {...listeners}>
          ⠿
        </button>
        <div className="todo-content" {...swipe}>
          {editing ? (
            <input
              className="todo-text"
              autoFocus
              value={value}
              onChange={(e) => {
                setValue(e.target.value);
                onText(e.target.value);
              }}
              onBlur={() => setEditing(false)}
              onKeyDown={(e) => {
                if (e.key === "Enter") setEditing(false);
              }}
            />
          ) : (
            <span className="todo-label" onDoubleClick={() => setEditing(true)}>
              {todo.text || "—"}
            </span>
          )}
        </div>
        <button className="edit" aria-label="edit" onClick={() => setEditing((v) => !v)}>
          ✎
        </button>
        {!isMobile && (
          <button className="danger" aria-label="delete" onClick={onDelete}>
            ✕
          </button>
        )}
      </div>
    </li>
  );
}
