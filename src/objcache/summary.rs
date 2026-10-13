//! The line a serving build ends its build step with (§18.12): how many
//! objects the cache gave of how many, and why the others were compiled.
//! `cactup __cc-summary <attempt>/cc` prints it from the event log, which is
//! all it reads (§18.1 rule 6).

use super::event::{self, Event, Miss, Outcome};
use super::events_path;
use std::path::Path;

/// Why a compile the cache did not serve was compiled (§18.12), in the
/// order the line names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Compiled {
    New,
    OtherSources,
    BuiltOtherwise,
    KeptOut,
    Failed,
}

impl Compiled {
    pub const ALL: [Compiled; 5] = [Compiled::New, Compiled::OtherSources, Compiled::BuiltOtherwise, Compiled::KeptOut, Compiled::Failed];

    /// What the line says after the count.
    pub fn words(self) -> &'static str {
        match self {
            Compiled::New => "new",
            Compiled::OtherSources => "with other sources",
            Compiled::BuiltOtherwise => "built otherwise",
            Compiled::KeptOut => "kept out",
            Compiled::Failed => "failed",
        }
    }
}

/// Why `event`'s compile ran, or `None` for one the cache served.
pub fn compiled(event: &Event) -> Option<Compiled> {
    // Served, or in audit mode checked (and compiled anyway, to compare).
    if event.outcome == Some(Outcome::Hit) {
        return None;
    }
    // Keyed in a build that only recorded: nothing was to be served.
    if event.outcome.is_none() && event.not_cached.is_none() {
        return None;
    }
    Some(match () {
        _ if event.signal.is_some() || event.exit.is_some_and(|exit| exit != 0) => Compiled::Failed,
        // No key, or an object that could not be stored: why is for `cache
        // doctor` to say.
        _ if event.not_cached.is_some() || event.stable == Some(false) || event.published == Some(false) => Compiled::KeptOut,
        _ => match event.miss {
            Some(Miss::New) => Compiled::New,
            Some(Miss::OtherSources) => Compiled::OtherSources,
            Some(Miss::BuiltOtherwise) => Compiled::BuiltOtherwise,
            Some(Miss::Unusable) => Compiled::KeptOut,
            // Keyed, not served, and no word of the store's records: the
            // store could not be used.
            None => Compiled::KeptOut,
        },
    })
}

/// The line, for a serving build's `events`. A kind with a count of 0 is
/// left out (Max, 2026-10-13).
pub fn line(events: &[Event]) -> String {
    let mut counts = [0usize; Compiled::ALL.len()];
    for kind in events.iter().filter_map(compiled) {
        counts[kind as usize] += 1;
    }
    let ran: usize = counts.iter().sum();
    let mut line = format!("cactup: build cache: {} of {} objects reused", events.len() - ran, events.len());
    if ran > 0 {
        let kinds: Vec<String> = Compiled::ALL.iter().zip(counts).filter(|(_, count)| *count > 0).map(|(kind, count)| format!("{count} {}", kind.words())).collect();
        line.push_str(&format!("; {ran} compiled: {}", kinds.join(", ")));
    }
    if counts[Compiled::KeptOut as usize] > 0 {
        line.push_str(" (see cactup cache doctor)");
    }
    line
}

/// `cactup __cc-summary <cc dir>`: print the line from the build's event
/// log. A log that cannot be read ends with status 1, and the build script
/// says what it can instead.
pub fn run(cc_dir: Option<std::ffi::OsString>) -> i32 {
    let Some(cc_dir) = cc_dir else { return 1 };
    match event::read(&events_path(Path::new(&cc_dir))) {
        Ok((events, _)) if !events.is_empty() => {
            println!("{}", line(&events));
            0
        }
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(outcome: Outcome, miss: Option<Miss>) -> Event {
        Event { outcome: Some(outcome), miss, exit: Some(0), stable: Some(true), key: Some("k".into()), ..Default::default() }
    }

    #[test]
    fn says_what_was_reused_and_why_the_rest_compiled_leaving_out_zero_counts() {
        let hit = event(Outcome::Hit, None);
        assert_eq!(line(&[hit.clone(), hit.clone()]), "cactup: build cache: 2 of 2 objects reused");
        let new = event(Outcome::Miss, Some(Miss::New));
        let other = event(Outcome::Miss, Some(Miss::OtherSources));
        assert_eq!(
            line(&[hit.clone(), new.clone(), new, other]),
            "cactup: build cache: 1 of 4 objects reused; 3 compiled: 2 new, 1 with other sources"
        );
        let kept = Event { not_cached: Some("it reads a specs file".into()), key: None, outcome: None, ..Default::default() };
        let failed = Event { exit: Some(1), ..event(Outcome::Miss, Some(Miss::New)) };
        let unstable = Event { stable: Some(false), ..event(Outcome::Miss, Some(Miss::BuiltOtherwise)) };
        assert_eq!(
            line(&[hit, kept, failed, unstable]),
            "cactup: build cache: 1 of 4 objects reused; 3 compiled: 2 kept out, 1 failed (see cactup cache doctor)"
        );
        // Checked in audit mode: found in the store, so reused, though it
        // was compiled anyway to compare; an entry that could not be used
        // is kept out.
        let audited = Event { audit: Some(event::Audit::Same), ..event(Outcome::Hit, None) };
        let unusable = Event { store: Some("the entry is damaged".into()), ..event(Outcome::Miss, Some(Miss::Unusable)) };
        assert_eq!(compiled(&audited), None);
        assert_eq!(compiled(&unusable), Some(Compiled::KeptOut));
        // A build that only recorded: nothing was to be served, but a
        // compile with no key was kept out all the same.
        let recorded = Event { key: Some("k".into()), exit: Some(0), ..Default::default() };
        assert_eq!(compiled(&recorded), None);
        assert_eq!(compiled(&Event { not_cached: Some("why".into()), ..Default::default() }), Some(Compiled::KeptOut));
    }
}
