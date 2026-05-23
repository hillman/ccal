// Raw-sync WebSocket client — the browser mirror of the server's `serve_peer`
// loop. It speaks the exact same protocol ccal-server does: raw
// `automerge::sync::Message` bytes, doc id in the path, token via the
// `ccal.bearer.<token>` subprotocol (browsers can't set Authorization). No
// envelope, no automerge-repo. A fresh SyncState per connection — the
// protocol re-derives from have-deps on reconnect, exactly like the server.

import * as Automerge from "@automerge/automerge";
import type { Doc } from "./schema";

export type ConnStatus = "connecting" | "online" | "offline";

export interface SyncCallbacks {
  /** Latest local doc — read on every flush so local edits are included. */
  getDoc(): Doc;
  /** Remote changes were merged; adopt the new doc. */
  setDoc(doc: Doc): void;
  onStatus(s: ConnStatus): void;
}

export interface SyncConn {
  /** Push local edits to the peer (call after a local change). */
  push(): void;
  close(): void;
}

export function connect(url: string, token: string, cb: SyncCallbacks): SyncConn {
  let ws: WebSocket | null = null;
  let sync = Automerge.initSyncState();
  let closed = false;
  let backoff = 500;

  const flush = () => {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const doc = cb.getDoc();
    for (;;) {
      const [next, msg] = Automerge.generateSyncMessage(doc, sync);
      sync = next;
      if (!msg) break;
      ws.send(msg);
    }
  };

  const open = () => {
    // No persisted per-peer state: re-derive from have-deps on every connect.
    sync = Automerge.initSyncState();
    cb.onStatus("connecting");
    ws = new WebSocket(url, [`ccal.bearer.${token}`]);
    ws.binaryType = "arraybuffer";
    ws.onopen = () => {
      backoff = 500;
      cb.onStatus("online");
      flush(); // kick the exchange: advertise our have-deps
    };
    ws.onmessage = (ev) => {
      const bytes = new Uint8Array(ev.data as ArrayBuffer);
      const [doc, next] = Automerge.receiveSyncMessage(cb.getDoc(), sync, bytes);
      sync = next;
      cb.setDoc(doc);
      flush(); // the protocol may owe a reply even when nothing changed
    };
    ws.onclose = () => {
      cb.onStatus("offline");
      retry();
    };
    ws.onerror = () => {
      try {
        ws?.close();
      } catch {
        /* close errors are not actionable */
      }
    };
  };

  const retry = () => {
    if (closed) return;
    setTimeout(open, backoff);
    backoff = Math.min(backoff * 2, 10_000);
  };

  open();

  return {
    push: flush,
    close() {
      closed = true;
      try {
        ws?.close();
      } catch {
        /* idempotent */
      }
    },
  };
}
