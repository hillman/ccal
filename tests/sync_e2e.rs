//! End-to-end: spawn the real `ccal-server` binary and sync two independent
//! `ccal::Store` replicas through it over WebSockets, exactly as the future
//! TUI client will. Also asserts the bearer-token gate.

use std::process::{Child, Command};
use std::time::Duration;

use ccal::{Store, SyncState};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "e2e-secret";

struct ServerGuard(Child);
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start_server(addr: &str) -> (ServerGuard, tempfile::TempDirShim) {
    let data = tempfile::TempDirShim::new();
    let child = Command::new(env!("CARGO_BIN_EXE_ccal-server"))
        .env("CCAL_SYNC_TOKEN", TOKEN)
        .env("CCAL_SYNC_ADDR", addr)
        .env("CCAL_SYNC_DATA", data.path())
        .spawn()
        .expect("spawn ccal-server");
    // Wait until it accepts connections.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return (ServerGuard(child), data);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("ccal-server did not come up");
}

fn req(addr: &str, token: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut r = format!("ws://{addr}/sync/ccal")
        .into_client_request()
        .unwrap();
    r.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    r
}

type Ws = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// Run the sync protocol for one connected replica to quiescence: send every
/// message the store wants to send, then absorb whatever the server pushes
/// back, until a round trip produces nothing in either direction.
async fn drive(ws: &mut Ws, store: &mut Store, state: &mut SyncState) {
    for _ in 0..100 {
        let mut activity = false;
        while let Some(bytes) = store.generate_sync_message(state) {
            ws.send(Message::Binary(bytes)).await.unwrap();
            activity = true;
        }
        // Drain anything the server has for us right now.
        loop {
            match tokio::time::timeout(Duration::from_millis(300), ws.next()).await {
                Ok(Some(Ok(Message::Binary(b)))) => {
                    store.receive_sync_message(state, &b).unwrap();
                    activity = true;
                }
                Ok(Some(Ok(_))) => {}
                _ => break, // timeout / closed: nothing more pending
            }
        }
        if !activity {
            return;
        }
    }
    panic!("sync did not converge");
}

#[tokio::test]
async fn rejects_bad_token() {
    let addr = "127.0.0.1:8791";
    let (_srv, _data) = start_server(addr).await;
    let err = tokio_tungstenite::connect_async(req(addr, "wrong")).await;
    assert!(err.is_err(), "handshake must fail with a bad token");
}

/// Browsers can't set `Authorization` on a WebSocket, so the token may arrive
/// via the `Sec-WebSocket-Protocol` subprotocol as `ccal.bearer.<token>`. The
/// server must accept it AND echo the exact protocol back per RFC 6455.
#[tokio::test]
async fn accepts_subprotocol_token_and_echoes_it() {
    let addr = "127.0.0.1:8793";
    let (_srv, _data) = start_server(addr).await;

    let mut r = format!("ws://{addr}/sync/ccal").into_client_request().unwrap();
    let proto = format!("ccal.bearer.{TOKEN}");
    r.headers_mut()
        .insert("sec-websocket-protocol", proto.parse().unwrap());

    let (_ws, resp) = tokio_tungstenite::connect_async(r)
        .await
        .expect("subprotocol token must be accepted");
    assert_eq!(
        resp.headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok()),
        Some(proto.as_str()),
        "server must echo the selected subprotocol",
    );
}

/// A wrong token in the subprotocol is still rejected.
#[tokio::test]
async fn rejects_bad_subprotocol_token() {
    let addr = "127.0.0.1:8794";
    let (_srv, _data) = start_server(addr).await;

    let mut r = format!("ws://{addr}/sync/ccal").into_client_request().unwrap();
    r.headers_mut()
        .insert("sec-websocket-protocol", "ccal.bearer.wrong".parse().unwrap());
    assert!(
        tokio_tungstenite::connect_async(r).await.is_err(),
        "a bad subprotocol token must be rejected",
    );
}

/// Documented fallback: the token may arrive as a `?token=` query param.
#[tokio::test]
async fn accepts_query_token() {
    let addr = "127.0.0.1:8795";
    let (_srv, _data) = start_server(addr).await;

    let r = format!("ws://{addr}/sync/ccal?token={TOKEN}")
        .into_client_request()
        .unwrap();
    assert!(
        tokio_tungstenite::connect_async(r).await.is_ok(),
        "query token must be accepted",
    );

    let bad = format!("ws://{addr}/sync/ccal?token=wrong")
        .into_client_request()
        .unwrap();
    assert!(
        tokio_tungstenite::connect_async(bad).await.is_err(),
        "a bad query token must be rejected",
    );
}

#[tokio::test]
async fn two_replicas_converge_through_server() {
    let addr = "127.0.0.1:8792";
    let (_srv, _data) = start_server(addr).await;
    let tmp = tempfile::TempDirShim::new();

    // Replica A creates a note and a todo, pushes to the server.
    let mut a = Store::open_at(tmp.path().join("a.automerge")).unwrap();
    let note_id = a.create_note(&["work".into()], "synced via server").unwrap();
    a.set_note_body(&note_id, "hello from A").unwrap();
    let todo_id = a.add_todo("ship sync").unwrap();

    let (mut wsa, _) = tokio_tungstenite::connect_async(req(addr, TOKEN)).await.unwrap();
    let mut sa = SyncState::new();
    drive(&mut wsa, &mut a, &mut sa).await;

    // Replica B starts empty, connects, and must receive A's state.
    let mut b = Store::open_at(tmp.path().join("b.automerge")).unwrap();
    let (mut wsb, _) = tokio_tungstenite::connect_async(req(addr, TOKEN)).await.unwrap();
    let mut sb = SyncState::new();
    drive(&mut wsb, &mut b, &mut sb).await;

    let n = b.note(&note_id).expect("note synced to B");
    assert_eq!(n.title, "synced via server");
    assert_eq!(n.body, "hello from A");
    assert!(b.todos().iter().any(|t| t.id == todo_id && t.text == "ship sync"));
}

/// Minimal temp-dir helper so the test needs no extra crate.
mod tempfile {
    use std::path::{Path, PathBuf};
    pub struct TempDirShim(PathBuf);
    impl TempDirShim {
        pub fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "ccal-e2e-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDirShim {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
