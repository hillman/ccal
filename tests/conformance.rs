//! P1 conformance — Rust side (proves JS -> Rust schema fidelity).
//!
//! The JS conformance driver (`web/test/conformance.ts`) writes a known note
//! and todos via the schema.ts helpers into a running ccal-server. This test
//! loads that on-disk replica through the real `Store` and asserts the same
//! values — so the TS and Rust schema implementations cannot silently drift.
//!
//! No-op unless `CCAL_CONFORMANCE_DOC_PATH` is set (so a plain `cargo test`
//! skips it); the orchestrator (`web/test/run-conformance.sh`) sets it.

use ccal::Store;

#[test]
fn js_written_doc_matches_rust_read() {
    let Ok(path) = std::env::var("CCAL_CONFORMANCE_DOC_PATH") else {
        eprintln!("conformance readback: CCAL_CONFORMANCE_DOC_PATH unset — skipping");
        return;
    };
    let store = Store::open_at(&path).expect("open server replica");

    // Note: title (Str), folder (List<Str>) and body (Text) as written by JS.
    let metas = store.note_metas();
    let meta = metas
        .iter()
        .find(|m| m.title == "P1 note edited")
        .unwrap_or_else(|| {
            panic!(
                "note not found; titles present: {:?}",
                metas.iter().map(|m| &m.title).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        meta.folder,
        vec!["work".to_string(), "proj".to_string()],
        "folder List<Str> mismatch",
    );
    let note = store.note(&meta.id).expect("full read of JS note");
    assert_eq!(note.body, "alpha bravo", "body Text mismatch");

    // Todos: the fractional reorder (move "third" to front) must yield the
    // same order the Rust `todos()` sort produces.
    let texts: Vec<String> = store.todos().into_iter().map(|t| t.text).collect();
    assert_eq!(
        texts,
        vec!["third".to_string(), "first".to_string(), "second".to_string()],
        "todo order via fractional index mismatch",
    );

    eprintln!("conformance readback: OK — Rust Store reads the JS-written doc identically");
}
