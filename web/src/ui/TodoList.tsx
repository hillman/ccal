import { useState } from "react";
import type { Store } from "../store";
import type { TodoView } from "../schema";

export function TodoList({ store, todos }: { store: Store; todos: TodoView[] }) {
  const [text, setText] = useState("");
  return (
    <div className="todos">
      <h2>Todos</h2>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (text.trim()) {
            store.addTodo(text.trim());
            setText("");
          }
        }}
      >
        <input
          value={text}
          placeholder="Add a todo…"
          onChange={(e) => setText(e.target.value)}
        />
      </form>
      <ul className="todo-list">
        {todos.map((t, i) => (
          <TodoItem
            key={t.id}
            todo={t}
            isFirst={i === 0}
            isLast={i === todos.length - 1}
            onText={(v) => store.setTodoText(t.id, v)}
            onUp={() => store.reorderTodo(t.id, i - 1)}
            onDown={() => store.reorderTodo(t.id, i + 1)}
            onDelete={() => store.deleteTodo(t.id)}
          />
        ))}
      </ul>
    </div>
  );
}

function TodoItem({
  todo,
  isFirst,
  isLast,
  onText,
  onUp,
  onDown,
  onDelete,
}: {
  todo: TodoView;
  isFirst: boolean;
  isLast: boolean;
  onText: (v: string) => void;
  onUp: () => void;
  onDown: () => void;
  onDelete: () => void;
}) {
  // Local value (keyed by id) so mid-text edits don't jump the cursor.
  const [value, setValue] = useState(todo.text);
  return (
    <li className="todo">
      <input
        className="todo-text"
        value={value}
        onChange={(e) => {
          setValue(e.target.value);
          onText(e.target.value);
        }}
      />
      <button title="move up" disabled={isFirst} onClick={onUp}>
        ↑
      </button>
      <button title="move down" disabled={isLast} onClick={onDown}>
        ↓
      </button>
      <button className="danger" title="delete" onClick={onDelete}>
        ✕
      </button>
    </li>
  );
}
