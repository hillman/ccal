//! Background sync against a `ccal-server` peer. Binary-private to the TUI;
//! like `app`/`ui` it talks only to `ccal::store` / `ccal::SyncState` and
//! never touches the `automerge` crate directly.
//!
//! Design: one OS thread, blocking `tungstenite` (no tokio in the TUI — the
//! "lib stays tokio-free" rule extends here). The doc is shared with the UI
//! thread via `Arc<Mutex<Store>>`. The store lock is held only for the brief
//! generate/receive/save calls — **never across network IO** — so a slow or
//! dead socket can't freeze the UI. After the handshake the socket is set
//! non-blocking (tungstenite resumes partial frames safely; a read timeout
//! could tear a frame mid-parse), so the loop also gets to push local edits
//! promptly instead of blocking on `read()`.
//!
//! Standalone is the same code path: if no `CCAL_SYNC_URL` is configured the
//! TUI simply never calls [`spawn`].
//!
//! v1 is `ws://` only (intended to run inside Tailscale, which provides the
//! encryption and authentication at the network layer). The bearer token is
//! still sent and checked so network-trust isn't the *only* gate.

use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tungstenite::client::IntoClientRequest;
use tungstenite::{Error as WsError, Message};

use ccal::models::now_ms;
use ccal::{Store, SyncState};

/// Shared handle the UI polls each tick.
pub struct Handle {
    /// Set by the thread when a remote change has been merged into the
    /// store; the UI swaps it false and rebuilds its views.
    pub dirty: Arc<AtomicBool>,
    /// One-line connection state for the status bar ("Synced", "Sync:
    /// offline, retrying…", …).
    pub status: Arc<Mutex<String>>,
    /// Epoch-ms of the last successful sync exchange (a protocol message
    /// sent or received). `0` = never synced since launch. The UI renders
    /// this as a persistent "synced Ns ago" indicator.
    pub last_sync: Arc<AtomicI64>,
    /// `true` while a socket is up and the handshake succeeded; `false`
    /// whenever the thread is offline / reconnecting.
    pub connected: Arc<AtomicBool>,
}

const POLL_IDLE: Duration = Duration::from_millis(100);
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Spawn the sync thread. `url` is e.g. `ws://host:8787/sync/ccal`.
pub fn spawn(store: Arc<Mutex<Store>>, url: String, token: String) -> Handle {
    let dirty = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new("Sync: connecting…".to_string()));
    let last_sync = Arc::new(AtomicI64::new(0));
    let connected = Arc::new(AtomicBool::new(false));
    let handle = Handle {
        dirty: dirty.clone(),
        status: status.clone(),
        last_sync: last_sync.clone(),
        connected: connected.clone(),
    };

    thread::Builder::new()
        .name("ccal-sync".into())
        .spawn(move || {
            let mut backoff = BACKOFF_START;
            loop {
                let r = session(
                    &store, &url, &token, &dirty, &status, &last_sync, &connected,
                );
                connected.store(false, Ordering::SeqCst);
                match r {
                    // Clean close (server restart, etc.) — reconnect fast.
                    Ok(()) => {
                        backoff = BACKOFF_START;
                        set(&status, "Sync: reconnecting…");
                        thread::sleep(BACKOFF_START);
                    }
                    Err(e) => {
                        set(&status, &format!("Sync: offline ({e}), retrying…"));
                        thread::sleep(backoff);
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                    }
                }
            }
        })
        .expect("spawn ccal-sync thread");

    handle
}

fn set(status: &Arc<Mutex<String>>, msg: &str) {
    if let Ok(mut s) = status.lock() {
        *s = msg.to_string();
    }
}

/// One connection attempt: handshake, then pump until the socket or a
/// protocol step errors. Returns `Ok` on a clean close.
#[allow(clippy::too_many_arguments)]
fn session(
    store: &Arc<Mutex<Store>>,
    url: &str,
    token: &str,
    dirty: &Arc<AtomicBool>,
    status: &Arc<Mutex<String>>,
    last_sync: &Arc<AtomicI64>,
    connected: &Arc<AtomicBool>,
) -> Result<(), String> {
    let mut req = url
        .into_client_request()
        .map_err(|e| format!("bad URL: {e}"))?;
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| "bad token".to_string())?,
    );

    let (mut ws, _resp) =
        tungstenite::connect(req).map_err(|e| describe(&e))?;

    // Non-blocking from here so `read()` yields instead of parking the
    // thread, letting us also flush locally-made edits every poll.
    match ws.get_ref() {
        tungstenite::stream::MaybeTlsStream::Plain(s) => s
            .set_nonblocking(true)
            .map_err(|e| format!("set_nonblocking: {e}"))?,
        _ => return Err("ws:// only in this build".into()),
    }

    let mut state = SyncState::new();
    set(status, "Sync: connected");
    connected.store(true, Ordering::SeqCst);

    loop {
        // 1. Push everything the protocol currently owes the server.
        loop {
            let next = {
                let mut st = store.lock().unwrap();
                st.generate_sync_message(&mut state)
            };
            match next {
                Some(bytes) => {
                    write(&mut ws, Message::Binary(bytes))?;
                    last_sync.store(now_ms(), Ordering::SeqCst);
                }
                None => break,
            }
        }
        if let Err(e) = ws.flush() {
            if !would_block(&e) {
                return classify(e);
            }
        }

        // 2. Drain whatever the server has for us right now.
        match ws.read() {
            Ok(Message::Binary(bytes)) => {
                last_sync.store(now_ms(), Ordering::SeqCst);
                let changed = {
                    let mut st = store.lock().unwrap();
                    st.receive_sync_message(&mut state, &bytes)
                        .map_err(|e| format!("apply: {e}"))?
                };
                if changed {
                    store
                        .lock()
                        .unwrap()
                        .save()
                        .map_err(|e| format!("save: {e}"))?;
                    dirty.store(true, Ordering::SeqCst);
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {} // ping/pong/text: tungstenite auto-handles pings
            Err(e) if would_block(&e) => thread::sleep(POLL_IDLE),
            Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => return Ok(()),
            Err(e) => return classify(e),
        }
    }
}

fn write(
    ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    msg: Message,
) -> Result<(), String> {
    match ws.write(msg) {
        Ok(()) => Ok(()),
        // Buffered internally; a later flush sends it.
        Err(e) if would_block(&e) => Ok(()),
        Err(e) => classify(e),
    }
}

fn would_block(e: &WsError) -> bool {
    matches!(e, WsError::Io(io)
        if io.kind() == ErrorKind::WouldBlock || io.kind() == ErrorKind::TimedOut)
}

fn classify(e: WsError) -> Result<(), String> {
    Err(describe(&e))
}

fn describe(e: &WsError) -> String {
    match e {
        WsError::Io(io) => format!("io: {io}"),
        WsError::Http(r) => format!("http {}", r.status()),
        other => other.to_string(),
    }
}
