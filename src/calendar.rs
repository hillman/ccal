//! Pure ICS → occurrences. **No network, no I/O, no Automerge** — same rule
//! as [`crate::models`]. Given the text of an `.ics` file and a `[start,
//! end)` window, it yields the concrete event occurrences in that window,
//! recurrences expanded.
//!
//! Recurrence/timezone *maths* is delegated to the `rrule` crate (a full
//! RFC 5545 engine) and parsing to `icalendar`. The only logic here is glue:
//! pull each `VEVENT`'s `DTSTART` + raw `RRULE`/`EXDATE`/`RDATE` strings,
//! hand them to a `RRuleSet`, and map the resulting instants to
//! [`Occurrence`]s. There is deliberately no calendar arithmetic, no
//! timezone code and no RRULE interpreter in this file.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use icalendar::{
    Calendar, CalendarComponent, CalendarDateTime, Component, DatePerhapsTime, EventLike,
};
use rrule::{RRuleSet, Tz};

/// One concrete event instance in the requested window. Times are absolute
/// UTC instants; the UI converts to local for display. All-day events carry
/// `all_day = true` and a midnight-UTC `start`/`end` whose *date* is what
/// matters (the clock part is not meaningful for them).
#[derive(Clone, Debug)]
pub struct Occurrence {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    pub summary: String,
    pub location: String,
    /// Display name of the calendar this came from (for multi-calendar
    /// agendas); filled in by the caller, not the ICS.
    pub calendar: String,
}

/// The calendar's self-declared display name (`X-WR-CALNAME`), if it set
/// one — used to auto-name a subscription the user didn't title.
pub fn calendar_name(ics: &str) -> Option<String> {
    let cal: Calendar = ics.parse().ok()?;
    cal.property_value("X-WR-CALNAME").map(|s| s.trim().to_string())
}

/// Expand every event in `ics` that touches `[window_start, window_end)`.
/// `calendar` labels each occurrence. Unparseable events are skipped rather
/// than failing the whole feed (real calendars routinely contain one odd
/// VEVENT); a feed that won't parse at all is the only hard error.
pub fn expand(
    ics: &str,
    calendar: &str,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Result<Vec<Occurrence>> {
    let cal: Calendar = ics
        .parse()
        .map_err(|e| anyhow!("parsing ICS: {e}"))?;

    let mut out = Vec::new();
    for comp in &cal.components {
        let CalendarComponent::Event(ev) = comp else {
            continue;
        };
        let Some(start_dpt) = ev.get_start() else {
            continue;
        };
        let (start, all_day) = to_utc(&start_dpt);
        // Duration carries across recurrences. Missing DTEND/all-day → a
        // 1-day block for all-day events, otherwise a zero-length point.
        let dur = match ev.get_end().map(|e| to_utc(&e).0) {
            Some(end) if end > start => end - start,
            _ if all_day => Duration::days(1),
            _ => Duration::zero(),
        };
        let summary = ev.get_summary().unwrap_or("(no title)").trim().to_string();
        let location = ev.get_location().unwrap_or("").trim().to_string();

        let mut push = |s: DateTime<Utc>| {
            let e = s + dur;
            // Half-open intersection: an event ending exactly at the window
            // start is in the past; one starting at window_end is out.
            if e > window_start && s < window_end {
                out.push(Occurrence {
                    start: s,
                    end: e,
                    all_day,
                    summary: summary.clone(),
                    location: location.clone(),
                    calendar: calendar.to_string(),
                });
            }
        };

        match ev.property_value("RRULE") {
            // Recurring: let `rrule` parse + expand. Build the minimal
            // iCalendar string it understands (DTSTART + rule lines).
            Some(rrule) => {
                let mut spec = format!("{}\nRRULE:{}", dtstart_line(&start_dpt), rrule.trim());
                if let Some(ex) = ev.property_value("EXDATE") {
                    spec.push_str(&format!("\nEXDATE:{}", ex.trim()));
                }
                if let Some(rd) = ev.property_value("RDATE") {
                    spec.push_str(&format!("\nRDATE:{}", rd.trim()));
                }
                let set: RRuleSet = match spec.parse() {
                    Ok(s) => s,
                    Err(_) => {
                        push(start); // unparseable rule: at least show the first instance
                        continue;
                    }
                };
                let res = set
                    .after(window_start.with_timezone(&Tz::UTC))
                    .before(window_end.with_timezone(&Tz::UTC))
                    .all(366);
                for dt in res.dates {
                    push(dt.with_timezone(&Utc));
                }
            }
            // One-shot event.
            None => push(start),
        }
    }

    out.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.summary.cmp(&b.summary)));
    Ok(out)
}

/// `DatePerhapsTime` → an absolute UTC instant plus an all-day flag. A
/// timezone we can't resolve, and floating local times, are read as UTC:
/// for a glance-view agenda a small offset on an exotic zone beats dropping
/// the event.
fn to_utc(dpt: &DatePerhapsTime) -> (DateTime<Utc>, bool) {
    match dpt {
        DatePerhapsTime::Date(d) => (midnight_utc(*d), true),
        DatePerhapsTime::DateTime(CalendarDateTime::Utc(dt)) => (*dt, false),
        DatePerhapsTime::DateTime(CalendarDateTime::Floating(ndt)) => {
            (Utc.from_utc_datetime(ndt), false)
        }
        DatePerhapsTime::DateTime(CalendarDateTime::WithTimezone { date_time, tzid }) => {
            match tzid.parse::<chrono_tz::Tz>() {
                Ok(tz) => (
                    tz.from_local_datetime(date_time)
                        .single()
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|| Utc.from_utc_datetime(date_time)),
                    false,
                ),
                Err(_) => (Utc.from_utc_datetime(date_time), false),
            }
        }
    }
}

fn midnight_utc(d: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("00:00:00 is valid"))
}

/// Reconstruct the `DTSTART` content line for the `rrule` parser, preserving
/// the value kind (date / utc / floating / tzid) since that decides how the
/// recurrence anchors.
fn dtstart_line(dpt: &DatePerhapsTime) -> String {
    match dpt {
        DatePerhapsTime::Date(d) => {
            format!("DTSTART;VALUE=DATE:{}", d.format("%Y%m%d"))
        }
        DatePerhapsTime::DateTime(CalendarDateTime::Utc(dt)) => {
            format!("DTSTART:{}", dt.format("%Y%m%dT%H%M%SZ"))
        }
        DatePerhapsTime::DateTime(CalendarDateTime::Floating(ndt)) => {
            format!("DTSTART:{}", ndt.format("%Y%m%dT%H%M%S"))
        }
        DatePerhapsTime::DateTime(CalendarDateTime::WithTimezone { date_time, tzid }) => {
            format!("DTSTART;TZID={tzid}:{}", date_time.format("%Y%m%dT%H%M%S"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn expands_weekly_recurrence_within_window() {
        // A Monday 09:00 UTC standup, weekly, indefinitely.
        let ics = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
X-WR-CALNAME:Work\r
BEGIN:VEVENT\r
UID:standup@example.com\r
DTSTART:20240101T090000Z\r
DTEND:20240101T093000Z\r
RRULE:FREQ=WEEKLY;BYDAY=MO\r
SUMMARY:Standup\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:oneoff@example.com\r
DTSTART:20240110T140000Z\r
DTEND:20240110T150000Z\r
SUMMARY:One-off review\r
END:VEVENT\r
END:VCALENDAR\r
";
        assert_eq!(calendar_name(ics).as_deref(), Some("Work"));

        // Window: the first full week of Jan 2024.
        let occ = expand(ics, "Work", at("2024-01-01T00:00:00Z"), at("2024-01-15T00:00:00Z"))
            .unwrap();

        // Mondays Jan 1 & 8 from the rule, plus the Jan 10 one-off = 3.
        let standups: Vec<_> = occ.iter().filter(|o| o.summary == "Standup").collect();
        assert_eq!(standups.len(), 2, "two Mondays in the window");
        assert_eq!(standups[0].start, at("2024-01-01T09:00:00Z"));
        assert_eq!(standups[1].start, at("2024-01-08T09:00:00Z"));
        assert!(occ.iter().any(|o| o.summary == "One-off review"));
        // Sorted by start.
        assert!(occ.windows(2).all(|w| w[0].start <= w[1].start));
        // Duration carried across recurrences (30 min).
        assert_eq!(standups[1].end - standups[1].start, Duration::minutes(30));
    }

    #[test]
    fn all_day_event_is_flagged() {
        let ics = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VEVENT\r
UID:hol@example.com\r
DTSTART;VALUE=DATE:20240320\r
SUMMARY:Holiday\r
END:VEVENT\r
END:VCALENDAR\r
";
        let occ = expand(ics, "Personal", at("2024-03-01T00:00:00Z"), at("2024-04-01T00:00:00Z"))
            .unwrap();
        assert_eq!(occ.len(), 1);
        assert!(occ[0].all_day);
        assert_eq!(occ[0].summary, "Holiday");
        assert_eq!(occ[0].end - occ[0].start, Duration::days(1));
    }
}
