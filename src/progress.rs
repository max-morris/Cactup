//! One rendered line per unit of work: a [`prodash::NestedProgress`] that
//! collapses a whole gix progress subtree onto a single prodash item.
//!
//! gix reports progress as a deep tree — a status line it renames per fetch
//! phase, a bar per phase underneath that, and per-thread delta-resolution
//! and decoding workers under *those* — and it narrates throughput as
//! `info` messages. Rendered verbatim (as cactup used to, four levels deep)
//! four concurrent component fetches produce a dozen bars appearing and
//! vanishing several times a second, and leave a wall of `done 12.3MB in
//! 1.2s`-style lines behind in the scrollback.
//!
//! A [`Line`] keeps **one** line per component instead (spec §2.4). It is
//! named for the component, labelled with the phase running now, and filled
//! by whichever phase is actually *moving* — the server's "Compressing
//! objects" while the pack is still being built, then the pack's bytes, then
//! the checkout's files — so the same bar carries a component the whole way
//! without ever splitting in two. gix's `info` chatter is dropped on the
//! floor: the only lines a run leaves in the scrollback are the ones cactup
//! writes itself, via [`Line::succeeded`], [`Line::warned`] and
//! [`Line::failed`].
//!
//! The renderer must be set up with a level filter that stops at the item
//! the `Line` is created over (`manifest::setup_prodash_with`) — the point
//! of collapsing the subtree is lost if the tree is drawn deeper than it.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use colored::Colorize;
use prodash::messages::MessageLevel;
use prodash::progress::{Id, Step, StepShared};
use prodash::Unit;

/// How deep below the handle handed to gix a child may still name the line
/// or fill its bar. Level 1 is gix's per-phase reporting (`remote`, `read
/// pack`, `create index file`, `checkout`); everything deeper is its
/// per-thread delta-resolution and decoding workers — several at once, each
/// with its own counter, none of them a phase anyone is waiting on by name.
const MAX_PHASE_DEPTH: u8 = 1;

/// What a progress child is allowed to do to the line it was created under.
#[derive(Clone, Copy)]
enum Role {
    /// Names the line *and* fills its bar from its own counter.
    Drive(Naming),
    /// Names the line; it has no counter of its own worth drawing (its
    /// numbers live in children we mute).
    Label(Naming),
    /// Ignored entirely — not even its name shows.
    Mute,
}

/// Where a phase's displayed name comes from.
#[derive(Clone, Copy)]
enum Naming {
    /// A fixed, friendlier name than gix's internal one.
    Fixed(&'static str),
    /// Whatever gix calls the child, live: the remote's sideband child is
    /// renamed as the server works ("Counting objects", then "Compressing
    /// objects", ...).
    FromGix,
}

/// Classify a progress child by the name its creator gave it. The names come
/// from gix-protocol, gix-pack and gix-worktree-state (and, for `checkout`
/// and `writing`, from [`crate::fetch::git::align`], which mirrors them).
///
/// An unrecognized child still gets to *name* the line but never draws a bar
/// of its own, so a gix upgrade that renames or adds a phase degrades to "no
/// bar during that phase" — never to a second competing bar, and never to a
/// nameless gap.
fn role_of(name: &str) -> Role {
    match name {
        // The server's sideband progress, forwarded verbatim.
        "remote" => Role::Drive(Naming::FromGix),
        // Pack bytes off the wire, bounded by the announced pack size.
        "read pack" => Role::Drive(Naming::Fixed("receiving pack")),
        // Index write and delta resolution. Its counters ("indexing",
        // "decompressing", "Resolving") are one level deeper, i.e. muted, so
        // this phase only names the line.
        "create index file" => Role::Label(Naming::Fixed("indexing pack")),
        // Files written into the worktree, bounded by the index entry count.
        "checkout" => Role::Drive(Naming::Fixed("checking out")),
        // The same checkout's bytes, counted unbounded. Two views of one
        // phase; the bounded one is the useful bar, so this one goes.
        "writing" => Role::Mute,
        _ => Role::Label(Naming::FromGix),
    }
}
/// How wide the name column may grow. A name longer than this is not
/// truncated — identifying the component matters more than the column — it
/// just pushes its own phase right, leaving that one line ragged instead of
/// costing every line the width.
const MAX_NAME_COLUMN: usize = 24;

/// The shared column layout for one batch of lines.
///
/// Every line puts its component's name in a column of the same width, so
/// the phase text — and with it the numbers and the bar prodash draws after
/// them — starts at the same place on every line, rather than sliding left
/// and right as the phases change underneath. Build one per phase (per
/// renderer) from the names it will carry.
#[derive(Clone, Copy)]
pub struct Layout {
    name: usize,
    colours: bool,
}

impl Layout {
    /// Sized to the widest name it will carry, capped at [`MAX_NAME_COLUMN`].
    pub fn for_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Layout {
        // `chars().count()`, not a display width: these are repo and
        // checkout names out of a thornlist, which are ASCII.
        let name = names.into_iter().map(|name| name.chars().count()).max().unwrap_or(0);
        Layout { name: name.min(MAX_NAME_COLUMN), colours: colours_on() }
    }

    /// A layout of a known width with colour off, so a test can assert the
    /// exact name an item is given.
    #[cfg(test)]
    fn plain(name: usize) -> Layout {
        Layout { name, colours: false }
    }

    /// The item name for a line that has no phase of its own — the headline
    /// over a batch. Laid out through the same path as the lines below it so
    /// its columns agree with theirs.
    pub fn headline(&self, name: &str) -> String {
        self.label(name, "")
    }

    /// The item name to wear while pushing a history line, so the dimmed
    /// name column of the scrollback lands under the name column of the
    /// bars above it, and the message text under their phase.
    ///
    /// prodash *right*-aligns a message's origin against the widest one it
    /// has seen, so every origin has to be the same width or one long name
    /// shunts every other line sideways; the leading space makes up the
    /// level indent the live lines get and this one does not.
    fn origin(&self, name: &str) -> String {
        let mut out = String::with_capacity(self.name + 3);
        out.push(' ');
        out.push_str(name);
        // One space past the column: prodash puts a single space between the
        // origin and the message, where a live line has two between the name
        // and the phase — so the text of both starts in the same place.
        for _ in 0..=self.name.saturating_sub(name.chars().count()) {
            out.push(' ');
        }
        out
    }

    /// `<name padded to the column>  <phase>`, with the name left louder
    /// than the phase.
    fn label(&self, name: &str, phase: &str) -> String {
        let mut out = String::with_capacity(name.len() + phase.len() + self.name + 8);
        out.push_str(name);
        for _ in 0..self.name.saturating_sub(name.chars().count()) {
            out.push(' ');
        }
        // The name is what a reader scans for, so it keeps the renderer's
        // emphasis (bold cyan) and everything after it drops to plain. That
        // has to be done from inside the string: prodash paints a task's
        // whole name in one style, and offers no hook to split it. `ESC [ m`
        // is the shortest sequence that ends the style — which matters,
        // because prodash measures the name to align the columns and cannot
        // tell that these three bytes will not print, so each one is a
        // column the bar gives up.
        if self.colours {
            out.push_str("\x1b[m");
        }
        if !phase.is_empty() {
            out.push_str("  ");
            out.push_str(phase);
        }
        out
    }
}

/// Whether the renderer will interpret colour escapes, read back from
/// prodash's own auto-configuration (terminal on stderr, `NO_COLOR`,
/// `CLICOLOR`) rather than guessed at again here — the two disagreeing would
/// mean raw escapes in a job log.
fn colours_on() -> bool {
    prodash::render::line::Options::default()
        .auto_configure(prodash::render::line::StreamKind::Stderr)
        .colored
}

/// The single prodash item every handle over it reports through.
struct Shared {
    /// Only `&self` methods are ever called on it: `add_child` is the one
    /// that needs `&mut`, and not adding children is the whole point.
    item: prodash::tree::Item,
    /// The stable name of the work the line is about (`cactusbase`) — what
    /// the line falls back to before any phase speaks up, and the origin
    /// every history line is attributed to.
    prefix: String,
    /// Whether the handle cactup itself holds may fill the bar. See
    /// [`Line::over`] and [`Line::counting`].
    root_draws: bool,
    layout: Layout,
    state: Mutex<State>,
}

struct State {
    /// Handed out in creation order (from 1, so that 0 can mean "the bar has
    /// never been claimed"), so a later phase always holds a higher token.
    next_token: u64,
    /// Every phase still alive, in creation order.
    phases: Vec<Phase>,
    /// The phase filling the bar right now.
    bar: Option<u64>,
    /// The highest token that ever filled the bar. Ownership only moves
    /// forward: a phase the work has moved past cannot take the bar back,
    /// which is what stops gix's sideband child — kept alive for the whole
    /// fetch, with object counts that go stale the moment the pack starts
    /// arriving — from fighting the pack reader for it.
    high_water: u64,
    /// The highest token that ever *named* the line, kept separately for the
    /// same reason: when a phase ends, the label passes to a newer phase
    /// still running, never back to an older one that has simply outlived
    /// it.
    label_high_water: u64,
    /// The name the handle cactup handed gix last set on itself, shown until
    /// a phase claims the line ("handshake", "negotiate (round 1)", ...).
    root_phase: String,
}

struct Phase {
    token: u64,
    /// Whether this phase may fill the bar, or only name the line.
    drives: bool,
    /// Whether a `set_name` from this phase is displayed, or ignored in
    /// favour of the fixed name [`role_of`] chose for it.
    live_name: bool,
    name: String,
    /// What this phase last announced with `init`, replayed onto the item
    /// when it takes the bar over. It is held rather than applied because a
    /// phase announces its shape long before it starts moving — gix opens
    /// the pack-reading phase while the server is still compressing objects
    /// — and the bar belongs to whatever is actually moving.
    announced: Option<(Option<Step>, Option<Unit>)>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("progress line poisoned")
    }

    /// Register a newly created phase, and return its token.
    fn add(&self, name: String, drives: bool, live_name: bool) -> u64 {
        let token = {
            let mut state = self.lock();
            let token = state.next_token;
            state.next_token += 1;
            state.phases.push(Phase { token, drives, live_name, name, announced: None });
            token
        };
        self.relabel(true);
        token
    }

    /// Retire a phase when its handle is dropped, clearing the bar if it was
    /// the one filling it.
    fn remove(&self, token: u64) {
        let cleared = {
            let mut state = self.lock();
            state.phases.retain(|phase| phase.token != token);
            let cleared = state.bar == Some(token);
            if cleared {
                state.bar = None;
            }
            cleared
        };
        if cleared {
            self.item.init(None, None);
        }
        // `false`: a finished phase must not hand the label back to the name
        // gix left on the root, which by then is behind the work — it still
        // says "receiving pack" while the pack is being resolved. The last
        // thing said stands until something new is said.
        self.relabel(false);
    }

    /// Whether `token` is filling the bar, taking it over first if the work
    /// has just moved on to it.
    ///
    /// The bar is claimed by a phase's first *movement*, not by its
    /// creation, and this is where that happens: it is called from the
    /// counting paths, never from `init`. Claiming replays the claimant's
    /// announced shape onto the item, which also resets the drawn count to
    /// zero, so the caller is told whether this call was the takeover and
    /// has to seed the count it already had.
    fn holds_bar(&self, token: u64) -> Claim {
        let announced = {
            let mut state = self.lock();
            if state.bar == Some(token) {
                return Claim::Drawing;
            }
            let claimable = state
                .phases
                .iter()
                .any(|phase| phase.token == token && phase.drives && token > state.high_water);
            if !claimable {
                return Claim::No;
            }
            state.bar = Some(token);
            state.high_water = token;
            state
                .phases
                .iter()
                .find(|phase| phase.token == token)
                .and_then(|phase| phase.announced.clone())
                .unwrap_or((None, None))
        };
        self.item.init(announced.0, announced.1);
        self.relabel(true);
        Claim::JustTaken
    }

    /// Remember (or, for the phase already drawing, apply) an `init`.
    fn announce(&self, token: u64, max: Option<Step>, unit: Option<Unit>) {
        let drawing = {
            let mut state = self.lock();
            let drawing = state.bar == Some(token);
            if let Some(phase) = state.phases.iter_mut().find(|phase| phase.token == token) {
                phase.announced = Some((max, unit.clone()));
            }
            drawing
        };
        if drawing {
            self.item.init(max, unit);
        }
    }

    fn announced(&self, token: u64) -> Option<(Option<Step>, Option<Unit>)> {
        self.lock()
            .phases
            .iter()
            .find(|phase| phase.token == token)
            .and_then(|phase| phase.announced.clone())
    }

    /// Leave a line in the scrollback. A message's dimmed origin column is
    /// the item's name at the moment it is pushed, so the phase decoration
    /// comes off first — it reads as the name of the work and nothing else —
    /// and the live label goes straight back on, since the fetch may still
    /// be running (a failure forwarded from the remote arrives mid-phase).
    fn push_message(&self, level: MessageLevel, message: String) {
        self.item.set_name(self.layout.origin(&self.prefix));
        self.item.message(level, message);
        self.relabel(true);
    }

    fn rename(&self, handle: &Handle, name: String) {
        {
            let mut state = self.lock();
            match handle {
                Handle::Root => state.root_phase = name,
                Handle::Phase(token) => {
                    match state.phases.iter_mut().find(|phase| phase.token == *token) {
                        Some(phase) if phase.live_name => phase.name = name,
                        _ => return,
                    }
                }
                Handle::Muted => return,
            }
        }
        self.relabel(true);
    }

    fn phase_name(&self, handle: &Handle) -> Option<String> {
        let state = self.lock();
        match handle {
            Handle::Root => Some(state.root_phase.clone()),
            Handle::Phase(token) => state
                .phases
                .iter()
                .find(|phase| phase.token == *token)
                .map(|phase| phase.name.clone()),
            Handle::Muted => None,
        }
    }

    /// `<prefix>  <phase>`, named for the phase filling the bar or — once
    /// that one is done — for the newest phase still running.
    /// `root_fallback` allows a drop back to the name gix left on the handle
    /// it holds, which reads right only before any phase has claimed the
    /// line.
    fn relabel(&self, root_fallback: bool) {
        let mut state = self.lock();
        let drawing = state
            .bar
            .and_then(|token| state.phases.iter().find(|phase| phase.token == token))
            .map(|phase| (phase.token, phase.name.clone()));
        let phase = match drawing {
            // Whatever is moving names the line, whichever phase it is.
            Some((token, name)) => {
                state.label_high_water = state.label_high_water.max(token);
                name
            }
            None => match state.phases.last() {
                // Nothing is moving: the newest phase still running names
                // it, unless the label has already moved past it (gix keeps
                // its sideband child alive long after the work has left it,
                // and "Compressing objects" must not come back around).
                Some(phase) if phase.token >= state.label_high_water => {
                    let (token, name) = (phase.token, phase.name.clone());
                    state.label_high_water = token;
                    name
                }
                Some(_) => return,
                None if root_fallback => state.root_phase.clone(),
                None => return,
            },
        };
        self.item.set_name(self.layout.label(&self.prefix, current_action(&phase)));
    }
}

/// What a counting handle learned about the bar when it wrote.
#[derive(PartialEq)]
enum Claim {
    /// It just took the bar over, which reset the drawn count to zero.
    JustTaken,
    /// It already had the bar.
    Drawing,
    /// The bar is someone else's.
    No,
}

/// Which handle of a [`Line`] this is — they differ only in what they are
/// allowed to write.
enum Handle {
    /// The handle cactup hands gix. It names the line until the phases take
    /// over, and — for a [`Line::counting`] line — fills the bar itself
    /// while no phase does.
    Root,
    /// A phase, identified by the token it was registered with.
    Phase(u64),
    /// A child whose reports go nowhere.
    Muted,
}

/// A progress handle that collapses everything reported through it — and
/// through every child it hands out — onto one prodash item. See the module
/// docs.
pub struct Line {
    shared: Arc<Shared>,
    handle: Handle,
    depth: u8,
    id: Id,
    /// This handle's own count, kept whether or not it is being drawn: it
    /// seeds the bar when the phase takes it over, and it is what `Count`
    /// reports back to a caller that is not being drawn.
    scratch: StepShared,
}

impl Line {
    /// Collapse a progress subtree onto `item`, whose name becomes
    /// `<prefix>` plus the phase currently running.
    ///
    /// The returned handle only ever *names* the line; the bar is filled by
    /// the phases reported underneath it. That is what a gix operation
    /// wants: gix counts a step or two on the handle it is given as it walks
    /// its own setup, and a bar reading "1 steps" between the phases that
    /// mean something is exactly the noise being removed here. Work that
    /// counts for itself, with no phases at all, wants [`Line::counting`].
    pub fn over(item: prodash::tree::Item, prefix: impl Into<String>, layout: Layout) -> Line {
        Line::new(item, prefix.into(), false, layout)
    }

    /// Like [`Line::over`], but the returned handle fills the bar with its
    /// own counter while no phase has claimed the line — for work that
    /// reports its progress directly, like a plain download.
    pub fn counting(item: prodash::tree::Item, prefix: impl Into<String>, layout: Layout) -> Line {
        Line::new(item, prefix.into(), true, layout)
    }

    fn new(item: prodash::tree::Item, prefix: String, root_draws: bool, layout: Layout) -> Line {
        item.set_name(layout.label(&prefix, ""));
        Line {
            shared: Arc::new(Shared {
                item,
                prefix,
                root_draws,
                layout,
                state: Mutex::new(State {
                    next_token: 1,
                    phases: Vec::new(),
                    bar: None,
                    high_water: 0,
                    label_high_water: 0,
                    root_phase: String::new(),
                }),
            }),
            handle: Handle::Root,
            depth: 0,
            id: prodash::progress::UNKNOWN,
            scratch: Default::default(),
        }
    }

    /// Name the work about to start on this line, for the stretch before the
    /// phases the operation itself reports: `"cloning"`, `"downloading"`.
    pub fn phase(&self, name: impl Into<String>) {
        self.shared.rename(&Handle::Root, name.into());
    }

    /// A green line in the scrollback: this unit of work finished.
    pub fn succeeded(&self, message: impl Into<String>) {
        self.history(MessageLevel::Success, message.into());
    }

    /// A yellow line in the scrollback: the work finished, but not
    /// untouched — local state was overwritten, a remote was re-pointed.
    pub fn warned(&self, message: impl Into<String>) {
        // prodash has exactly three message levels and none of them is a
        // warning (Info draws white, Success green, Failure red), so the
        // colour has to travel inside the message text. `colored` turns
        // itself off when the output is not a terminal (and honours
        // NO_COLOR), which is what keeps escapes out of piped job logs.
        self.history(MessageLevel::Info, message.into().yellow().to_string());
    }

    /// A red line in the scrollback: this unit of work failed. The caller
    /// still reports the failure in its own summary — this is what makes it
    /// visible *live*, next to the work that was going on around it.
    pub fn failed(&self, message: impl Into<String>) {
        self.history(MessageLevel::Failure, message.into());
    }

    fn history(&self, level: MessageLevel, message: String) {
        self.shared.push_message(level, message);
    }

    /// Whether this handle's counts reach the drawn bar — taking the bar
    /// over for this handle's phase if the work has just moved on to it.
    fn claim(&self) -> Claim {
        match self.handle {
            // Nothing has claimed the bar, so it is the root's to fill —
            // if it was created as a counting line at all.
            Handle::Root if self.shared.root_draws && self.shared.lock().bar.is_none() => Claim::Drawing,
            Handle::Phase(token) => self.shared.holds_bar(token),
            _ => Claim::No,
        }
    }

    /// Whether this handle's counts reach the drawn bar.
    fn drives(&self) -> bool {
        self.claim() != Claim::No
    }

    fn child(&self, name: String, id: Id) -> Line {
        let depth = self.depth.saturating_add(1);
        let role = if depth <= MAX_PHASE_DEPTH { role_of(&name) } else { Role::Mute };
        let handle = match role {
            Role::Mute => Handle::Muted,
            Role::Drive(naming) | Role::Label(naming) => {
                let drives = matches!(role, Role::Drive(_));
                let (phase, live_name) = match naming {
                    Naming::Fixed(fixed) => (fixed.to_string(), false),
                    Naming::FromGix => (name, true),
                };
                Handle::Phase(self.shared.add(phase, drives, live_name))
            }
        };
        Line { shared: Arc::clone(&self.shared), handle, depth, id, scratch: Default::default() }
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        if let Handle::Phase(token) = self.handle {
            self.shared.remove(token);
        }
    }
}

impl prodash::Count for Line {
    fn set(&self, step: Step) {
        self.scratch.store(step, Ordering::Relaxed);
        if self.drives() {
            self.shared.item.set(step);
        }
    }

    fn step(&self) -> Step {
        self.scratch.load(Ordering::Relaxed)
    }

    fn inc_by(&self, step: Step) {
        // Mirrored with `set`, not `inc_by`: taking the bar over resets the
        // drawn count to zero, so this handle's own total is the truth.
        let total = self.scratch.fetch_add(step, Ordering::Relaxed) + step;
        if self.drives() {
            self.shared.item.set(total);
        }
    }

    fn counter(&self) -> StepShared {
        // Handing out the item's own counter is how gix's checkout reports
        // at all — it takes the counter and increments it directly, never
        // through this handle — so asking for it counts as the movement that
        // claims the bar. Only the takeover seeds the count: a second caller
        // asking for the same counter must not reset a bar already filling
        // through it.
        match self.claim() {
            Claim::JustTaken => {
                self.shared.item.set(self.scratch.load(Ordering::Relaxed));
                prodash::Count::counter(&self.shared.item)
            }
            Claim::Drawing => prodash::Count::counter(&self.shared.item),
            Claim::No => Arc::clone(&self.scratch),
        }
    }
}

impl prodash::Progress for Line {
    fn init(&mut self, max: Option<Step>, unit: Option<Unit>) {
        match self.handle {
            // Held, not applied, until this phase starts moving: see
            // `Shared::holds_bar`.
            Handle::Phase(token) => self.shared.announce(token, max, unit),
            Handle::Root if self.shared.root_draws => self.shared.item.init(max, unit),
            _ => {}
        }
    }

    fn unit(&self) -> Option<Unit> {
        match self.handle {
            Handle::Phase(token) => self.shared.announced(token).and_then(|(_, unit)| unit),
            Handle::Root if self.shared.root_draws => self.shared.item.unit(),
            _ => None,
        }
    }

    fn max(&self) -> Option<Step> {
        match self.handle {
            Handle::Phase(token) => self.shared.announced(token).and_then(|(max, _)| max),
            Handle::Root if self.shared.root_draws => self.shared.item.max(),
            _ => None,
        }
    }

    fn set_max(&mut self, max: Option<Step>) -> Option<Step> {
        let previous = prodash::Progress::max(self);
        let unit = prodash::Progress::unit(self);
        prodash::Progress::init(self, max, unit);
        previous
    }

    fn set_name(&mut self, name: String) {
        self.shared.rename(&self.handle, name);
    }

    fn name(&self) -> Option<String> {
        self.shared.phase_name(&self.handle)
    }

    fn id(&self) -> Id {
        self.id
    }

    fn message(&self, level: MessageLevel, message: String) {
        // gix's own narration is the noise this type exists to remove:
        // per-phase throughput summaries, "writing index file", and a `done`
        // line for every one of them. A failure is not noise — that one gets
        // through, and is how the remote's stderr reaches the user at all.
        if level == MessageLevel::Failure {
            self.shared.push_message(level, message);
        }
    }
}

impl prodash::NestedProgress for Line {
    type SubProgress = Line;

    fn add_child(&mut self, name: impl Into<String>) -> Line {
        self.child(name.into(), prodash::progress::UNKNOWN)
    }

    fn add_child_with_id(&mut self, name: impl Into<String>, id: Id) -> Line {
        self.child(name.into(), id)
    }
}

/// gix's sideband naming *chains* the server's actions with `": "`
/// ("Counting objects: Compressing objects"), so a long fetch would grow an
/// ever-longer label. Only the action actually running is interesting.
fn current_action(phase: &str) -> &str {
    phase.rsplit(':').next().unwrap_or(phase).trim()
}

/// A byte count for a history line: `4.2 MiB`, or bytes below a KiB.
pub fn bytes(n: usize) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = UNITS[0];
    for next in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!("{value:.1} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use prodash::{Count, NestedProgress, Progress, Root};

    type Snapshot = Vec<(prodash::progress::key::Level, String, Option<(Step, Option<Step>)>)>;

    /// The tree the renderer would draw, as `(level, name, step/max)`.
    fn snapshot(root: &Arc<prodash::tree::Root>) -> Snapshot {
        let mut out = Vec::new();
        root.sorted_snapshot(&mut out);
        out.into_iter()
            .map(|(key, task)| {
                (
                    key.level(),
                    task.name.clone(),
                    task.progress.map(|p| (p.step.load(Ordering::Relaxed), p.done_at)),
                )
            })
            .collect()
    }

    fn messages(root: &Arc<prodash::tree::Root>) -> Vec<(MessageLevel, String, String)> {
        let mut out = Vec::new();
        root.copy_messages(&mut out);
        out.into_iter().map(|m| (m.level, m.origin, m.message)).collect()
    }

    #[test]
    fn a_gix_subtree_collapses_onto_one_item() {
        let root = prodash::tree::Root::new();
        let mut line = Line::over(root.add_child("headline"), "cactusbase", Layout::plain(10));
        line.phase("cloning");
        assert_eq!(snapshot(&root), vec![(1, "cactusbase  cloning".into(), None)]);

        // gix renames the item it was handed as the fetch proceeds, and adds
        // its own children: none of that may grow the drawn tree.
        line.set_name("negotiate (round 1)".into());
        let mut remote = line.add_child("remote");
        remote.set_name("Counting objects".into());
        remote.init(Some(100), None);
        remote.set(40);
        assert_eq!(
            snapshot(&root),
            vec![(1, "cactusbase  Counting objects".into(), Some((40, Some(100))))]
        );
    }

    #[test]
    fn the_newest_phase_owns_the_line_and_stale_ones_cannot_write() {
        let root = prodash::tree::Root::new();
        let mut line = Line::over(root.add_child("headline"), "cactusbase", Layout::plain(10));
        let mut remote = line.add_child("remote");
        remote.init(Some(100), None);
        remote.set(40);

        // gix keeps the sideband child alive for the whole fetch; once the
        // pack arrives its counts are stale and must not be drawn.
        let mut pack = line.add_child("read pack");
        pack.init(Some(2048), None);
        pack.inc_by(512);
        remote.set(99);
        assert_eq!(
            snapshot(&root),
            vec![(1, "cactusbase  receiving pack".into(), Some((512, Some(2048))))]
        );

        // When the phase ends the bar is cleared, and the line falls back to
        // the name gix last gave the handle it holds.
        line.set_name("receiving pack".into());
        drop(pack);
        assert_eq!(snapshot(&root), vec![(1, "cactusbase  receiving pack".into(), None)]);
        // ... and the still-live sideband child cannot resurrect its bar.
        remote.set(100);
        assert_eq!(snapshot(&root), vec![(1, "cactusbase  receiving pack".into(), None)]);
    }

    #[test]
    fn a_bounded_phase_is_not_displaced_by_its_unbounded_twin() {
        let root = prodash::tree::Root::new();
        let mut line = Line::over(root.add_child("headline"), "cactusbase", Layout::plain(10));
        let mut files = line.add_child("checkout");
        let mut written = line.add_child("writing");
        files.init(Some(1200), None);
        written.init(None, None);
        files.set(300);
        written.inc_by(4096);
        assert_eq!(
            snapshot(&root),
            vec![(1, "cactusbase  checking out".into(), Some((300, Some(1200))))]
        );
    }

    #[test]
    fn a_second_ask_for_the_counter_does_not_reset_the_bar() {
        // gix's checkout counts through the counter it takes, not through
        // the handle, so the takeover is the only moment that may seed it.
        let root = prodash::tree::Root::new();
        let mut line = Line::over(root.add_child("headline"), "cactusbase", Layout::plain(10));
        let mut files = line.add_child("checkout");
        files.init(Some(1200), None);
        let counter = Count::counter(&files);
        counter.fetch_add(700, Ordering::Relaxed);
        assert_eq!(
            snapshot(&root),
            vec![(1, "cactusbase  checking out".into(), Some((700, Some(1200))))]
        );
        drop(Count::counter(&files));
        assert_eq!(
            snapshot(&root),
            vec![(1, "cactusbase  checking out".into(), Some((700, Some(1200))))]
        );
    }

    #[test]
    fn per_thread_workers_are_muted() {
        let root = prodash::tree::Root::new();
        let mut line = Line::over(root.add_child("headline"), "cactusbase", Layout::plain(10));
        let mut index = line.add_child("create index file");
        let mut resolving = index.add_child("Resolving");
        resolving.init(Some(9000), None);
        resolving.set(4500);
        // The phase names the line; its per-thread counters draw nothing.
        assert_eq!(snapshot(&root), vec![(1, "cactusbase  indexing pack".into(), None)]);
    }

    #[test]
    fn gix_chatter_is_dropped_but_failures_and_our_own_lines_survive() {
        let root = prodash::tree::Root::new();
        let mut line = Line::over(root.add_child("headline"), "cactusbase", Layout::plain(10));
        let mut pack = line.add_child("read pack");
        pack.init(Some(2048), Some(prodash::unit::label("bytes")));
        pack.inc_by(2048);
        pack.show_throughput(std::time::Instant::now());
        pack.info("writing index file".into());
        pack.done("done 2048 bytes".into());
        assert_eq!(messages(&root), vec![]);

        pack.fail("remote: repository is read-only".into());
        // The fetch is still running, so the live label goes straight back
        // on after the line is pushed.
        assert_eq!(
            snapshot(&root),
            vec![(1, "cactusbase  receiving pack".into(), Some((2048, Some(2048))))]
        );
        drop(pack);
        line.succeeded("cloned at 0123456789ab");
        assert_eq!(
            messages(&root),
            vec![
                // Every origin is one width, live label or not, so a long
                // name cannot shunt the whole column sideways.
                (MessageLevel::Failure, " cactusbase ".into(), "remote: repository is read-only".into()),
                (MessageLevel::Success, " cactusbase ".into(), "cloned at 0123456789ab".into()),
            ]
        );
    }

    #[test]
    fn a_counting_line_fills_the_bar_from_its_own_handle() {
        // What a plain download needs: no phases at all, one bar.
        let root = prodash::tree::Root::new();
        let mut line = Line::counting(root.add_child("headline"), "flesh.tar.gz", Layout::plain(12));
        line.phase("downloading");
        line.init(Some(4096), None);
        line.inc_by(1024);
        assert_eq!(
            snapshot(&root),
            vec![(1, "flesh.tar.gz  downloading".into(), Some((1024, Some(4096))))]
        );
        assert_eq!(Count::step(&line), 1024);
    }

    #[test]
    fn a_gix_line_draws_no_bar_of_its_own() {
        // gix counts a step or two on the handle it is given while walking
        // its own setup; "1 steps" is not a phase anyone is waiting on.
        let root = prodash::tree::Root::new();
        let mut line = Line::over(root.add_child("headline"), "cactusbase", Layout::plain(10));
        line.phase("cloning");
        line.init(Some(4), Some(prodash::unit::label("steps")));
        line.inc();
        assert_eq!(snapshot(&root), vec![(1, "cactusbase  cloning".into(), None)]);
    }

    #[test]
    fn the_bar_belongs_to_whichever_phase_is_moving() {
        let root = prodash::tree::Root::new();
        let mut line = Line::over(root.add_child("headline"), "cactusbase", Layout::plain(10));
        // gix opens both phases up front: the sideband child, and the pack
        // reader that will not see a byte until the server stops
        // compressing. Announcing a shape is not moving, so neither draws.
        let mut remote = line.add_child("remote");
        let mut pack = line.add_child("read pack");
        remote.init(Some(900), None);
        pack.init(None, Some(prodash::unit::label("bytes")));
        assert_eq!(snapshot(&root), vec![(1, "cactusbase  receiving pack".into(), None)]);

        // The server's progress is what is moving, so it gets the bar even
        // though the pack reader was created after it.
        remote.set_name("Compressing objects".into());
        remote.set(300);
        assert_eq!(
            snapshot(&root),
            vec![(1, "cactusbase  Compressing objects".into(), Some((300, Some(900))))]
        );

        // Then the bytes start, and the bar is theirs for good.
        pack.inc_by(4096);
        remote.set(900);
        assert_eq!(snapshot(&root), vec![(1, "cactusbase  receiving pack".into(), Some((4096, None)))]);
    }

    #[test]
    fn a_streaming_phase_keeps_the_bar_and_hands_over_only_the_label() {
        let root = prodash::tree::Root::new();
        let mut line = Line::over(root.add_child("headline"), "cactusbase", Layout::plain(10));
        line.set_name("receiving pack".into());
        let mut pack = line.add_child("read pack");
        pack.init(None, Some(prodash::unit::label("bytes")));
        pack.inc_by(4096);

        // gix opens the indexing phase while the pack is still streaming
        // into it: the bytes are what is moving, so they keep the bar.
        let index = line.add_child("create index file");
        pack.inc_by(4096);
        assert_eq!(snapshot(&root), vec![(1, "cactusbase  receiving pack".into(), Some((8192, None)))]);

        // Once the bytes stop, the label follows the work still running —
        // never back to "receiving pack", which the root still says.
        drop(pack);
        assert_eq!(snapshot(&root), vec![(1, "cactusbase  indexing pack".into(), None)]);
        drop(index);
        assert_eq!(snapshot(&root), vec![(1, "cactusbase  indexing pack".into(), None)]);
    }

    #[test]
    fn the_name_column_is_the_same_width_on_every_line() {
        let layout = Layout { name: Layout::for_names(["uv", "gitoxide"]).name, colours: false };
        // Padded to the widest name, so every phase — and so every value
        // column and bar — starts at the same place.
        let uv = layout.label("uv", "receiving pack");
        let gitoxide = layout.label("gitoxide", "receiving pack");
        assert_eq!(uv, "uv        receiving pack");
        assert_eq!(gitoxide, "gitoxide  receiving pack");
        assert_eq!(uv.find("receiving"), gitoxide.find("receiving"));

        // A name wider than the column pushes only its own phase right,
        // rather than costing every line that width.
        let wide = "a-component-name-longer-than-the-column";
        assert!(wide.chars().count() > MAX_NAME_COLUMN);
        let layout = Layout { name: Layout::for_names(["uv", wide]).name, colours: false };
        assert_eq!(layout.name, MAX_NAME_COLUMN);
        assert_eq!(layout.label(wide, "checking out"), format!("{wide}  checking out"));
    }

    #[test]
    fn the_phase_is_quieter_than_the_name() {
        // prodash paints a task's whole name in one style, so the style has
        // to be ended inside the string, right after the name column.
        let coloured = Layout { name: 8, colours: true };
        assert_eq!(coloured.label("uv", "receiving pack"), "uv      \x1b[m  receiving pack");
        assert_eq!(coloured.headline("uv"), "uv      \x1b[m");
        // With colour off (a pipe, NO_COLOR), no escape is emitted at all —
        // a job log gets the columns and nothing else.
        let plain = Layout::plain(8);
        assert_eq!(plain.label("uv", "receiving pack"), "uv        receiving pack");
        assert_eq!(plain.headline("uv"), "uv      ");
    }

    #[test]
    fn byte_counts_read_as_sizes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
