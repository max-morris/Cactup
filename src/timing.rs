//! Opt-in timing for diagnosing slow filesystems. With `CACTUP_TIMING` set
//! (to anything but empty or `0`), cactup records how long its named phases
//! take and how often its filesystem-heavy operations run, and prints the
//! tally to stderr when the command ends. On NFS/Lustre every lock cycle,
//! clock probe and status walk is a chain of synchronous round trips, so
//! these numbers are what tell a slow server apart from too many trips.
//!
//! An environment variable, not a knob: it has to work before the database
//! is read (the database read is one of the things it measures). Unset, the
//! whole module is a single cached boolean check per call site — nothing is
//! allocated or locked.

use std::ffi::OsStr;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

static ENABLED: LazyLock<bool> =
    LazyLock::new(|| enabled_by(std::env::var_os("CACTUP_TIMING").as_deref()));

/// When the process started measuring: set by [`start`], the first thing
/// `main` does.
static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// Timed scopes: `total` is what matters.
    Span,
    /// Events with an amount each (e.g. files read by one walk): `sum` is
    /// what matters, and zero is a real answer.
    Counter,
}

/// One row of the tally, in first-recorded order.
struct Row {
    name: &'static str,
    kind: Kind,
    count: u64,
    total: Duration,
    sum: u64,
}

static ROWS: Mutex<Vec<Row>> = Mutex::new(Vec::new());

fn enabled_by(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

pub fn enabled() -> bool {
    *ENABLED
}

/// Start the process clock (when timing is on).
pub fn start() {
    if enabled() {
        LazyLock::force(&STARTED);
    }
}

/// Time the enclosing scope as `name`. Spans of one name accumulate.
pub fn span(name: &'static str) -> Span {
    Span { name, started: enabled().then(Instant::now) }
}

/// Record one occurrence of `name`, contributing `amount` to its sum.
pub fn count(name: &'static str, amount: u64) {
    if enabled() {
        record(name, Kind::Counter, Duration::ZERO, amount);
    }
}

pub struct Span {
    name: &'static str,
    started: Option<Instant>,
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            record(self.name, Kind::Span, started.elapsed(), 0);
        }
    }
}

fn record(name: &'static str, kind: Kind, elapsed: Duration, amount: u64) {
    let mut rows = ROWS.lock().unwrap_or_else(|e| e.into_inner());
    match rows.iter_mut().find(|r| r.name == name) {
        Some(row) => {
            debug_assert_eq!(row.kind, kind, "{name} recorded as both a span and a counter");
            row.count += 1;
            row.total += elapsed;
            row.sum += amount;
        }
        None => rows.push(Row { name, kind, count: 1, total: elapsed, sum: amount }),
    }
}

/// Print the tally to stderr (when timing is on).
pub fn report() {
    if !enabled() {
        return;
    }
    let rows = ROWS.lock().unwrap_or_else(|e| e.into_inner());
    eprint!("\n{}", format_rows(&rows, STARTED.elapsed()));
}

fn format_rows(rows: &[Row], total: Duration) -> String {
    use std::fmt::Write;
    let width = rows.iter().map(|r| r.name.len()).max().unwrap_or(0).max("total".len());
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    // Spans recorded on parallel workers add up across threads, so a row can
    // exceed the wall-clock total.
    let mut out = "cactup timing (CACTUP_TIMING; worker-thread spans sum across threads):\n".to_owned();
    let _ = writeln!(out, "  {:<width$}  {:>6}  {:>10.1} ms", "total", "", ms(total));
    for row in rows {
        let calls = format!("{}x", row.count);
        let _ = match row.kind {
            Kind::Span => {
                writeln!(out, "  {:<width$}  {calls:>6}  {:>10.1} ms", row.name, ms(row.total))
            }
            Kind::Counter => writeln!(out, "  {:<width$}  {calls:>6}  {:>10}", row.name, row.sum),
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_rule() {
        assert!(!enabled_by(None));
        assert!(!enabled_by(Some(OsStr::new(""))));
        assert!(!enabled_by(Some(OsStr::new("0"))));
        assert!(enabled_by(Some(OsStr::new("1"))));
        assert!(enabled_by(Some(OsStr::new("yes"))));
    }

    #[test]
    fn record_accumulates_by_name() {
        record("accumulate", Kind::Span, Duration::from_millis(2), 0);
        record("accumulate", Kind::Span, Duration::from_millis(3), 0);
        let rows = ROWS.lock().unwrap();
        let row = rows.iter().find(|r| r.name == "accumulate").unwrap();
        assert_eq!((row.count, row.total), (2, Duration::from_millis(5)));
    }

    #[test]
    fn counters_print_their_sum_even_when_zero() {
        let row = |name, kind, total, sum| Row { name, kind, count: 2, total, sum };
        let rows = [
            row("walk", Kind::Span, Duration::from_millis(7), 0),
            row("racy", Kind::Counter, Duration::ZERO, 0),
            row("read", Kind::Counter, Duration::ZERO, 12),
        ];
        let text = format_rows(&rows, Duration::from_millis(9));
        let line =
            |name: &str| text.lines().find(|l| l.trim_start().starts_with(name)).unwrap().to_owned();
        assert!(line("walk").ends_with("7.0 ms"), "{text}");
        assert!(line("racy").ends_with(" 0"), "{text}");
        assert!(line("read").ends_with(" 12"), "{text}");
        assert!(line("total").ends_with("9.0 ms"), "{text}");
    }
}
