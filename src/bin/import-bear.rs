//! Standalone, one-shot importer: Bear's SQLite DB -> the ccal Automerge
//! store. Completely separate from the TUI app; the only shared surface is
//! `ccal::store`. Run once, then you live in ccal's world — Bear's keys are
//! NOT reused as identity (the store mints its own UUIDs).
//!
//! Bear's DB is read-only: it (plus any WAL/SHM) is snapshotted to a temp
//! dir and queried via the system `sqlite3 -json`. Trashed, archived and
//! encrypted notes are skipped. A note is filed under its most specific
//! tag; nested tags (`a/b`) become nested folders; untagged -> `Untagged`.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use ccal::models::NoteInput;
use ccal::store::Store;

/// Core Data timestamps are seconds since 2001-01-01 UTC.
const CORE_DATA_OFFSET: f64 = 978_307_200.0;
const BEAR_REL: &str =
    "Library/Group Containers/9K33E3U3T4.net.shinyfrog.bear/Application Data/database.sqlite";

fn bear_db() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("HOME")?).join(BEAR_REL);
    p.exists().then_some(p)
}

fn query(db: &Path, sql: &str) -> Result<Vec<Value>> {
    let out = Command::new("sqlite3")
        .arg("-json")
        .arg(db)
        .arg(sql)
        .output()
        .context("running sqlite3 (is it on PATH?)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "sqlite3 failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_str(text)? {
        Value::Array(rows) => Ok(rows),
        _ => Ok(Vec::new()),
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
fn i(v: &Value, k: &str) -> i64 {
    v.get(k)
        .and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(0)
}
fn fnum(v: &Value, k: &str) -> Option<f64> {
    v.get(k)
        .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
}

/// More specific = more `/` segments, then longer string.
fn most_specific(tags: &[String]) -> Option<&String> {
    tags.iter()
        .max_by(|a, b| a.matches('/').count().cmp(&b.matches('/').count()).then(a.len().cmp(&b.len())))
}

fn folder_for(tag: Option<&String>) -> Vec<String> {
    match tag {
        Some(t) => {
            let parts: Vec<String> = t
                .split('/')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect();
            if parts.is_empty() {
                vec!["Untagged".into()]
            } else {
                parts
            }
        }
        None => vec!["Untagged".into()],
    }
}

fn note_name(title: &str, body: &str) -> String {
    let t = title.trim();
    if !t.is_empty() {
        return t.lines().next().unwrap_or(t).to_string();
    }
    body.lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("untitled")
        .to_string()
}

fn ms(v: &Value, k: &str) -> i64 {
    fnum(v, k)
        .map(|secs| ((secs + CORE_DATA_OFFSET) * 1000.0) as i64)
        .unwrap_or(0)
}

fn main() -> Result<()> {
    let db = bear_db().context("Bear database not found for this user")?;

    // Snapshot db + sidecars; never touch the original.
    let tmp = std::env::temp_dir().join("ccal-bear-import");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let snap = tmp.join("database.sqlite");
    std::fs::copy(&db, &snap).context("copying Bear database")?;
    for ext in ["-wal", "-shm"] {
        let side = db.with_file_name(format!("database.sqlite{ext}"));
        if side.exists() {
            let _ = std::fs::copy(&side, tmp.join(format!("database.sqlite{ext}")));
        }
    }

    let tag_title: HashMap<i64, String> =
        query(&snap, "SELECT Z_PK AS pk, IFNULL(ZTITLE,'') AS title FROM ZSFNOTETAG")?
            .iter()
            .map(|r| (i(r, "pk"), s(r, "title")))
            .collect();

    let mut note_tags: HashMap<i64, Vec<String>> = HashMap::new();
    for r in query(&snap, "SELECT Z_5NOTES AS note, Z_13TAGS AS tag FROM Z_5TAGS")? {
        if let Some(t) = tag_title.get(&i(&r, "tag")) {
            if !t.is_empty() {
                note_tags.entry(i(&r, "note")).or_default().push(t.clone());
            }
        }
    }

    let notes = query(
        &snap,
        "SELECT Z_PK AS pk, IFNULL(ZTITLE,'') AS title, IFNULL(ZTEXT,'') AS text, \
         ZCREATIONDATE AS created, IFNULL(ZMODIFICATIONDATE, ZCREATIONDATE) AS modified \
         FROM ZSFNOTE \
         WHERE ZTRASHED=0 AND ZARCHIVED=0 AND IFNULL(ZENCRYPTED,0)=0",
    )?;

    let skipped = i(
        query(
            &snap,
            "SELECT count(*) AS n FROM ZSFNOTE \
             WHERE ZTRASHED!=0 OR ZARCHIVED!=0 OR IFNULL(ZENCRYPTED,0)!=0",
        )?
        .first()
        .unwrap_or(&Value::Null),
        "n",
    );

    let batch: Vec<NoteInput> = notes
        .iter()
        .map(|n| {
            let pk = i(n, "pk");
            let body = s(n, "text");
            let title = s(n, "title");
            let tags = note_tags.get(&pk).cloned().unwrap_or_default();
            NoteInput {
                folder: folder_for(most_specific(&tags)),
                title: note_name(&title, &body),
                body,
                created: ms(n, "created"),
                modified: ms(n, "modified"),
            }
        })
        .collect();

    let total = batch.len();
    let bytes: usize = batch.iter().map(|n| n.body.len()).sum();
    eprintln!(
        "Read {total} notes from Bear ({:.1} MB of text), {skipped} skipped. Building store…",
        bytes as f64 / 1_048_576.0
    );

    let mut store = Store::open().context("opening ccal store")?;
    let t = Instant::now();
    let imported = store
        .import_notes(&batch, |done| {
            if done % 25 == 0 || done == total {
                let rate = done as f64 / t.elapsed().as_secs_f64().max(0.001);
                eprintln!("  {done}/{total} notes  ({rate:.0}/s)");
                let _ = std::io::stderr().flush();
            }
        })
        .context("importing notes")?;
    eprintln!("\r  {imported}/{total} notes written in {:.1}s", t.elapsed().as_secs_f64());

    let ts = Instant::now();
    store.save().context("saving ccal store")?;
    eprintln!("  saved in {:.1}s", ts.elapsed().as_secs_f64());
    let _ = std::fs::remove_dir_all(&tmp);

    println!("Bear import complete: {imported} imported, {skipped} skipped (trashed/archived/encrypted).");
    Ok(())
}
