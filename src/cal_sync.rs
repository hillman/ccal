//! Background calendar refresh. Binary-private to the TUI, same shape and
//! rules as [`crate::sync_client`]: one OS thread, blocking IO (`ureq`, no
//! tokio — the "lib stays tokio-free" rule extends here), the `Store` lock
//! held only for brief calls, **never across network IO**.
//!
//! What syncs vs. what doesn't: the *subscription list* lives in the shared
//! Automerge doc (so it reaches every device and the server, via the
//! existing `sync_client`). The *events* do not — this thread fetches each
//! ICS over HTTPS, expands recurrences with `ccal::calendar`, and publishes
//! the result into an in-memory cache the UI reads. Nothing event-shaped is
//! ever written to the doc, so its history can't bloat from 5-minute
//! refreshes.
//!
//! Standalone-friendly: with no subscriptions the loop just idles cheaply.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use chrono::Utc;

use ccal::calendar::{self, Occurrence};
use ccal::models::now_ms;
use ccal::Store;

/// Per-subscription fetch outcome, surfaced in the calendar manager so a
/// broken feed (401, DNS, parse error) is visible without blanking the
/// agenda.
#[derive(Clone)]
pub struct CalStatus {
    pub id: String,
    pub name: String,
    /// Epoch-ms of the last successful fetch (0 = never since launch).
    pub last_ok: i64,
    /// `Some` describes the most recent failure; `None` once it succeeds.
    pub error: Option<String>,
    pub events: usize,
}

/// Shared handle the UI polls each tick (mirrors `sync_client::Handle`).
pub struct Handle {
    /// All calendars' occurrences in the window, merged and sorted by start.
    pub occurrences: Arc<Mutex<Vec<Occurrence>>>,
    pub statuses: Arc<Mutex<Vec<CalStatus>>>,
    /// Thread sets it true after publishing a fresh cache; the UI swaps it
    /// false and redraws.
    pub dirty: Arc<AtomicBool>,
    /// UI sets it true to demand an immediate refresh (after add/remove, or
    /// the `r` key); the thread swaps it false and refetches now.
    pub refresh_now: Arc<AtomicBool>,
}

/// How long before/after "now" to expand. Slightly over a month forward
/// covers today + this week with slack; one day back keeps an event that
/// started yesterday and runs into today.
const BACK: i64 = 1;
const FORWARD: i64 = 35;

/// HTTP timeout per feed — generous (mobile tailnet) but bounded so one
/// dead host can't stall the cycle for the others.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Poll granularity while waiting out the refresh interval, so a forced
/// refresh is picked up promptly without a busy loop.
const TICK: Duration = Duration::from_millis(200);

pub fn spawn(store: Arc<Mutex<Store>>, interval: Duration) -> Handle {
    let occurrences = Arc::new(Mutex::new(Vec::new()));
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let dirty = Arc::new(AtomicBool::new(false));
    let refresh_now = Arc::new(AtomicBool::new(false));
    let handle = Handle {
        occurrences: occurrences.clone(),
        statuses: statuses.clone(),
        dirty: dirty.clone(),
        refresh_now: refresh_now.clone(),
    };

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .user_agent("ccal-calendar")
        .build()
        .into();

    thread::Builder::new()
        .name("ccal-cal".into())
        .spawn(move || loop {
            refresh(&store, &agent, &occurrences, &statuses);
            dirty.store(true, Ordering::SeqCst);

            // Wait out the interval, but wake early on a forced refresh.
            let ticks = (interval.as_millis() / TICK.as_millis()).max(1);
            for _ in 0..ticks {
                if refresh_now.swap(false, Ordering::SeqCst) {
                    break;
                }
                thread::sleep(TICK);
            }
        })
        .expect("spawn ccal-cal thread");

    handle
}

/// One refresh cycle: snapshot the subscriptions (brief lock), then fetch +
/// expand each with the lock released.
fn refresh(
    store: &Arc<Mutex<Store>>,
    agent: &ureq::Agent,
    occurrences: &Arc<Mutex<Vec<Occurrence>>>,
    statuses: &Arc<Mutex<Vec<CalStatus>>>,
) {
    let cals = store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .calendars();

    let now = Utc::now();
    let start = now - chrono::Duration::days(BACK);
    let end = now + chrono::Duration::days(FORWARD);

    let mut all: Vec<Occurrence> = Vec::new();
    let mut stats: Vec<CalStatus> = Vec::new();

    for cal in &cals {
        let mut st = CalStatus {
            id: cal.id.clone(),
            name: if cal.name.is_empty() { cal.url.clone() } else { cal.name.clone() },
            last_ok: 0,
            error: None,
            events: 0,
        };
        match fetch(agent, &cal.url) {
            Ok(ics) => {
                // Backfill a missing name from the feed itself, once. This
                // is a doc write, so it rides the normal sync path.
                if cal.name.is_empty() {
                    if let Some(name) = calendar::calendar_name(&ics).filter(|n| !n.is_empty()) {
                        let mut g = store.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = g.set_calendar_name(&cal.id, &name);
                        let _ = g.save();
                        st.name = name;
                    }
                }
                match calendar::expand(&ics, &st.name, start, end) {
                    Ok(mut occ) => {
                        st.events = occ.len();
                        st.last_ok = now_ms();
                        all.append(&mut occ);
                    }
                    Err(e) => st.error = Some(format!("parse: {e}")),
                }
            }
            Err(e) => st.error = Some(e),
        }
        stats.push(st);
    }

    // Real feeds dupe heavily: the same event subscribed via two calendars,
    // and recurring master/override pairs. Collapse anything identical in
    // start, end and summary (sort so exact dupes are adjacent, then
    // `dedup_by` keeps the first). Cross-calendar dupes are intentionally
    // merged — a glance agenda wants one row, not "in 2 calendars".
    all.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| a.end.cmp(&b.end))
            .then_with(|| a.summary.cmp(&b.summary))
    });
    all.dedup_by(|a, b| {
        a.start == b.start && a.end == b.end && a.summary == b.summary
    });

    *occurrences.lock().unwrap_or_else(|e| e.into_inner()) = all;
    *statuses.lock().unwrap_or_else(|e| e.into_inner()) = stats;
}

/// Blocking HTTPS GET of one ICS feed. Errors are short, human strings (the
/// calendar manager shows them inline).
fn fetch(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let mut resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("fetch: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    resp.body_mut()
        .read_to_string()
        .map_err(|e| format!("read: {e}"))
}
