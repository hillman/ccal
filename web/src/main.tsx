import { createRoot } from "react-dom/client";
import { App } from "./ui/App";
import "./ui/styles.css";

// No <StrictMode>: it double-invokes effects in dev, which would open two
// sync WebSockets. The single-connection model is simpler to reason about.
createRoot(document.getElementById("root")!).render(<App />);
