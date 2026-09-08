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
//! named for the component, labeled with the phase running now, and filled
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

/// The least width the bar keeps: this many cells, or [`BAR_SHARE`] of the
/// terminal, whichever is more. The text column gets what is left.
const MIN_BAR_COLUMN: usize = 20;

/// The share of the terminal's width the bar takes, as a fraction:
/// `(numerator, denominator)`.
const BAR_SHARE: (usize, usize) = (1, 3);

/// What a line under a headline hangs from it by: `├─ ` for every line but
/// the last, which gets the corner. Three columns, counted against the
/// text column like the rest of the line.
const BRANCH: &str = "├─ ";
const LAST_BRANCH: &str = "└─ ";

/// How often the numbers on every live line are rewritten. gix's checkout
/// counts through the counter it takes rather than through the handle, so
/// nothing but a clock can keep its numbers current; the renderer draws at
/// 6 frames a second, so anything faster than this is wasted.
const REFRESH: std::time::Duration = std::time::Duration::from_millis(100);

/// The shared column layout for one batch of lines.
///
/// A line is `<name>  <phase> <numbers>`, anchored left, then the bar,
/// anchored right — and the bar is the same width on every line, because
/// the text is padded (or clipped) to one width before prodash gets to
/// draw after it. prodash's own layout is name, then numbers right-aligned
/// against the widest line, then a bar of whatever is left: the numbers
/// drift away from their phase and the bars change width as the phases
/// change underneath. So cactup composes the numbers into the text itself
/// and gives prodash a unit that prints nothing, which leaves it the bar
/// alone. Build one layout per phase (per renderer) from the names it will
/// carry, and hand a clone to every line.
#[derive(Clone)]
pub struct Layout {
    /// The name column: every line's name is padded to it.
    name: usize,
    /// The text column: every line's `<name>  <phase> <numbers>` is padded
    /// or clipped to it, so the bar after it starts in one place.
    text: usize,
    colors: bool,
    refresh: Arc<Refresh>,
}

impl Layout {
    /// Sized to the widest name it will carry, capped at [`MAX_NAME_COLUMN`],
    /// and to the terminal the renderer will draw on.
    pub fn for_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Layout {
        // `chars().count()`, not a display width: these are repo and
        // checkout names out of a thornlist, which are ASCII.
        let name = names.into_iter().map(|name| name.chars().count()).max().unwrap_or(0);
        let (colors, columns) = terminal();
        Layout { refresh: Refresh::ticking(), ..Layout::fit(name, columns, colors) }
    }

    /// Fit the columns into a terminal `columns` wide: the bar takes
    /// [`BAR_SHARE`] of it (at least [`MIN_BAR_COLUMN`]) and the text gets
    /// the rest, so a line never reaches the edge — a wrapped line throws
    /// off the renderer's cursor arithmetic for every frame after it.
    fn fit(name: usize, columns: usize, colors: bool) -> Layout {
        let name = name.min(MAX_NAME_COLUMN);
        let bar = MIN_BAR_COLUMN.max(columns * BAR_SHARE.0 / BAR_SHARE.1);
        // Around the text, prodash draws: two columns of level indent; the
        // color escape, which it measures as if it printed; two spaces
        // where its (empty) numbers go; ` [`, the bar, and `]`.
        let escape = if colors { 3 } else { 0 };
        let text = columns.saturating_sub(bar + escape + 7);
        Layout { name, text, colors, refresh: Arc::new(Refresh::default()) }
    }

    /// A layout of a known width with color off, no terminal to fit and no
    /// clock — a test rewrites the numbers itself with [`Layout::tick`].
    #[cfg(test)]
    fn plain(name: usize) -> Layout {
        Layout { name, text: 64, colors: false, refresh: Arc::new(Refresh::default()) }
    }

    /// Rewrite the numbers on every live line now.
    #[cfg(test)]
    fn tick(&self) {
        self.refresh.tick()
    }

    /// The item name for one line: `<branch><name>  <phase> <numbers>`,
    /// padded to the text column. `branch` is what hangs the line from the
    /// headline above it (see [`Refresh::mark_last`]), or nothing for a
    /// line with no headline.
    fn text(&self, branch: &str, name: &str, phase: &str, numbers: &str) -> String {
        self.compose(branch, name, phase, numbers, self.text)
    }

    /// The item name for the headline over a batch, given the `values` it
    /// counts in (`43/80 components`). It sits one level above its lines —
    /// one column less of indent — so it is padded one wider to put its
    /// bar in their column.
    fn headline(&self, name: &str, values: &str) -> String {
        self.compose("", name, "", values, self.text + 1)
    }

    /// `<branch><name padded to the name column>  <phase> <numbers>`,
    /// padded to `width` — or clipped to it, with an ellipsis, when the
    /// phase and its numbers say more than the column holds. A name wider
    /// than its column is never clipped (identifying the component matters
    /// most); it eats into the room its own phase has.
    fn compose(&self, branch: &str, name: &str, phase: &str, numbers: &str, width: usize) -> String {
        let mut out = String::from(branch);
        out.push_str(&self.field(name, self.name));
        let used = branch.chars().count() + self.name.max(name.chars().count());
        let mut rest = String::with_capacity(phase.len() + numbers.len() + 1);
        rest.push_str(phase);
        if !phase.is_empty() && !numbers.is_empty() {
            rest.push(' ');
        }
        rest.push_str(numbers);
        let room = width.saturating_sub(used + 2);
        let mut shown = rest.chars().count();
        if shown > room {
            rest = rest.chars().take(room.saturating_sub(1)).collect();
            if room > 0 {
                rest.push('…');
            }
            shown = rest.chars().count();
        }
        let mut filled = used;
        if !rest.is_empty() {
            out.push_str("  ");
            out.push_str(&rest);
            filled += 2 + shown;
        }
        for _ in filled..width {
            out.push(' ');
        }
        out
    }

    /// The item name to wear while pushing a history line, so the dimmed
    /// name column of the scrollback lands under the name column of the
    /// bars above it, and the message text under their phase.
    ///
    /// prodash *right*-aligns a message's origin against the widest one it
    /// has seen, so every origin has to be the same width or one long name
    /// shunts every other line sideways; the leading space makes up the
    /// level indent the live lines get and this one does not, and a line
    /// that hung from a headline leaves the width of its branch blank — the
    /// headline is below it now, not above.
    fn origin(&self, branched: bool, name: &str) -> String {
        let mut out = String::with_capacity(self.name + 6);
        out.push(' ');
        if branched {
            for _ in 0..BRANCH.chars().count() {
                out.push(' ');
            }
        }
        out.push_str(name);
        // One space past the column: prodash puts a single space between the
        // origin and the message, where a live line has two between the name
        // and the phase — so the text of both starts in the same place.
        for _ in 0..=self.name.saturating_sub(name.chars().count()) {
            out.push(' ');
        }
        out
    }

    /// `name` padded to `width`, then the end of the renderer's style.
    fn field(&self, name: &str, width: usize) -> String {
        let mut out = String::with_capacity(width + 4);
        out.push_str(name);
        for _ in 0..width.saturating_sub(name.chars().count()) {
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
        if self.colors {
            out.push_str("\x1b[m");
        }
        out
    }
}

/// What the renderer will do with the terminal — whether it will interpret
/// color escapes, and how many columns it will lay lines out in — read back
/// from prodash's own auto-configuration (terminal on stderr, `NO_COLOR`,
/// `CLICOLOR`, the tty's size) rather than guessed at again here: the two
/// disagreeing would mean raw escapes in a job log, or columns sized for a
/// terminal other than the one being drawn on.
fn terminal() -> (bool, usize) {
    let options = prodash::render::line::Options::default()
        .auto_configure(prodash::render::line::StreamKind::Stderr);
    (options.colored, options.terminal_dimensions.0 as usize)
}

/// A unit that prints nothing, so that prodash draws a line's bar and
/// nothing else after its name — the numbers are in the name, put there by
/// [`Shared::paint`]. The bar's fill still comes from the item's own step
/// and bound, which are set exactly as before.
struct Blank;

impl prodash::unit::DisplayValue for Blank {
    fn display_current_value(
        &self,
        _: &mut dyn std::fmt::Write,
        _: Step,
        _: Option<Step>,
    ) -> std::fmt::Result {
        Ok(())
    }

    fn separator(&self, _: &mut dyn std::fmt::Write, _: Step, _: Option<Step>) -> std::fmt::Result {
        Ok(())
    }

    fn display_upper_bound(&self, _: &mut dyn std::fmt::Write, _: Step, _: Step) -> std::fmt::Result {
        Ok(())
    }

    fn dyn_hash(&self, state: &mut dyn std::hash::Hasher) {
        state.write_u8(0)
    }

    fn display_unit(&self, _: &mut dyn std::fmt::Write, _: Step) -> std::fmt::Result {
        Ok(())
    }
}

fn blank() -> Unit {
    prodash::unit::dynamic(Blank)
}

/// The numbers for a bar of shape `(max, unit)` at `step`, as prodash would
/// have drawn them: `4096/8192 bytes [50%] |1.2MB/s|` — the unit decides
/// which of those it shows.
fn numbers(step: Step, max: Option<Step>, unit: Option<&Unit>, per_second: Option<Step>) -> String {
    match unit {
        Some(unit) => {
            let rate = per_second.map(|rate| {
                prodash::unit::display::Throughput::new(rate, std::time::Duration::from_secs(1))
            });
            unit.display(step, max, rate).to_string()
        }
        None => match max {
            Some(max) => format!("{step}/{max}"),
            None => step.to_string(),
        },
    }
}

/// The clock that keeps every line's numbers current: one thread per
/// layout, rewriting each live line every [`REFRESH`], and gone as soon as
/// the last clone of the layout is. The lines are held weakly, so a
/// finished line is not kept alive (or repainted) by it.
#[derive(Default)]
struct Refresh {
    lines: Mutex<Vec<std::sync::Weak<Shared>>>,
}

impl Refresh {
    fn ticking() -> Arc<Refresh> {
        let refresh = Arc::new(Refresh::default());
        let weak = Arc::downgrade(&refresh);
        std::thread::Builder::new()
            .name("progress refresh".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(REFRESH);
                    match weak.upgrade() {
                        Some(refresh) => refresh.tick(),
                        None => break,
                    }
                }
            })
            .expect("failed to spawn the progress refresh thread");
        refresh
    }

    fn register(&self, line: &Arc<Shared>) {
        self.lines.lock().expect("progress refresh poisoned").push(Arc::downgrade(line));
        self.mark_last();
    }

    /// The live lines, oldest first, with the dead ones forgotten.
    fn live(&self) -> Vec<Arc<Shared>> {
        let mut lines = self.lines.lock().expect("progress refresh poisoned");
        lines.retain(|line| line.strong_count() > 0);
        lines.iter().filter_map(std::sync::Weak::upgrade).collect()
    }

    fn tick(&self) {
        self.mark_last();
        for line in self.live() {
            line.paint();
        }
    }

    /// Give the corner to the line drawn last under the headline — the one
    /// with the highest order, since the headline numbers its lines in the
    /// order prodash draws them — and repaint whichever lines that changed.
    /// Called when a line arrives or leaves, and on every tick, so the
    /// corner is never more than a beat behind.
    fn mark_last(&self) {
        let lines = self.live();
        let last = lines.iter().filter_map(|line| line.order).max();
        for line in lines {
            let mine = line.order.is_some() && line.order == last;
            if line.last.swap(mine, Ordering::Relaxed) != mine {
                line.paint();
            }
        }
    }
}

/// A count's rate of change, sampled a second at a time — what prodash's
/// own throughput tracker would have shown after the numbers, had it been
/// drawing them.
#[derive(Default)]
struct Rate {
    since: Option<(std::time::Instant, Step)>,
    per_second: Option<Step>,
}

impl Rate {
    fn sample(&mut self, step: Step, now: std::time::Instant) -> Option<Step> {
        match self.since {
            None => self.since = Some((now, step)),
            Some((at, then)) => {
                let elapsed = now.saturating_duration_since(at);
                if elapsed >= std::time::Duration::from_secs(1) {
                    self.per_second =
                        Some((step.saturating_sub(then) as f64 / elapsed.as_secs_f64()) as Step);
                    self.since = Some((now, step));
                }
            }
        }
        self.per_second
    }
}

/// The headline over a batch of lines — `fetch components  43/80
/// components` — counting the lines finished under it.
pub struct Headline {
    item: prodash::tree::Item,
    /// The order of the next line under it, carried in the item's id so
    /// the line can read it back: this is what [`Refresh::mark_last`] sorts
    /// by, and it is assigned here — under the headline's own `&mut` — so
    /// it agrees with the order prodash draws the lines in, which lines
    /// registering themselves from four threads at once could not promise.
    next: u32,
    name: String,
    unit: &'static str,
    total: usize,
    done: usize,
    layout: Layout,
}

impl Headline {
    /// Turn `item` into the headline over `total` units of work, counted in
    /// `unit` (`"components"`).
    pub fn over(
        item: prodash::tree::Item,
        name: impl Into<String>,
        total: usize,
        unit: &'static str,
        layout: Layout,
    ) -> Headline {
        item.init(Some(total), Some(blank()));
        let headline = Headline { item, next: 1, name: name.into(), unit, total, done: 0, layout };
        headline.relabel();
        headline
    }

    /// The item for one line under the headline.
    pub fn add_child(&mut self, name: impl Into<String>) -> prodash::tree::Item {
        let order = self.next;
        self.next += 1;
        self.item.add_child_with_id(name, order.to_be_bytes())
    }

    /// One more unit of work finished.
    pub fn inc(&mut self) {
        self.done += 1;
        self.item.inc();
        self.relabel();
    }

    fn relabel(&self) {
        let values = format!("{}/{} {}", self.done, self.total, self.unit);
        self.item.set_name(self.layout.headline(&self.name, &values));
    }
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
    /// The rate the drawn count is moving at, for the numbers.
    rate: Mutex<Rate>,
    /// Where this line comes under its headline, or `None` for a line with
    /// no headline over it (the manifest's). See [`Headline::add_child`].
    order: Option<u32>,
    /// Whether this is the last line under the headline, and so hangs from
    /// it by the corner.
    last: std::sync::atomic::AtomicBool,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // This line is gone from the tree the moment its last handle is:
        // pass the corner on now rather than a tick later.
        self.layout.refresh.mark_last();
    }
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
    /// The phase the line is labeled with right now.
    label: String,
    /// The shape of the bar being drawn — its bound and its unit, which
    /// decides how the numbers read — or `None` while there is no bar.
    drawn: Option<(Option<Step>, Option<Unit>)>,
}

struct Phase {
    token: u64,
    /// Whether this phase may fill the bar, or only name the line.
    drives: bool,
    /// Whether a `set_name` from this phase is displayed, or ignored in
    /// favor of the fixed name [`role_of`] chose for it.
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
            self.draw(None, None);
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
        self.draw(announced.0, announced.1);
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
            self.draw(max, unit);
        }
    }

    /// Put a bar of shape `(max, unit)` on the item — or none, if it has
    /// neither. prodash gets the bound, which is what fills the bar, and a
    /// unit that prints nothing; the real unit is kept for the numbers.
    fn draw(&self, max: Option<Step>, unit: Option<Unit>) {
        let shape = (max.is_some() || unit.is_some()).then_some((max, unit));
        self.lock().drawn = shape.clone();
        *self.rate.lock().expect("progress rate poisoned") = Rate::default();
        self.item.init(max, shape.map(|_| blank()));
        self.paint();
    }

    /// Rewrite the item's name from what the line says and where its count
    /// is: `<name>  <phase> <numbers>`, padded to the text column.
    fn paint(&self) {
        let (label, drawn) = {
            let state = self.lock();
            (state.label.clone(), state.drawn.clone())
        };
        let numbers = match drawn {
            None => String::new(),
            Some((max, unit)) => {
                let step = self.item.step().unwrap_or(0);
                let per_second = self
                    .rate
                    .lock()
                    .expect("progress rate poisoned")
                    .sample(step, std::time::Instant::now());
                numbers(step, max, unit.as_ref(), per_second)
            }
        };
        self.item
            .set_name(self.layout.text(self.branch(), &self.prefix, &label, &numbers));
    }

    /// What this line hangs from its headline by.
    fn branch(&self) -> &'static str {
        match self.order {
            None => "",
            Some(_) if self.last.load(Ordering::Relaxed) => LAST_BRANCH,
            Some(_) => BRANCH,
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
        self.item.set_name(self.layout.origin(self.order.is_some(), &self.prefix));
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
        state.label = current_action(&phase).to_string();
        drop(state);
        self.paint();
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
        let order = match item.id() {
            prodash::progress::UNKNOWN => None,
            id => Some(u32::from_be_bytes(id)),
        };
        let shared = Arc::new(Shared {
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
                label: String::new(),
                drawn: None,
            }),
            rate: Mutex::new(Rate::default()),
            order,
            last: std::sync::atomic::AtomicBool::new(false),
        });
        shared.layout.refresh.register(&shared);
        shared.paint();
        Line {
            shared,
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
        // color has to travel inside the message text. `colored` turns
        // itself off when the output is not a terminal (and honors
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
            Handle::Root if self.shared.root_draws => self.shared.draw(max, unit),
            _ => {}
        }
    }

    fn unit(&self) -> Option<Unit> {
        match self.handle {
            Handle::Phase(token) => self.shared.announced(token).and_then(|(_, unit)| unit),
            Handle::Root if self.shared.root_draws => {
                self.shared.lock().drawn.clone().and_then(|(_, unit)| unit)
            }
            _ => None,
        }
    }

    fn max(&self) -> Option<Step> {
        match self.handle {
            Handle::Phase(token) => self.shared.announced(token).and_then(|(max, _)| max),
            Handle::Root if self.shared.root_draws => {
                self.shared.lock().drawn.clone().and_then(|(max, _)| max)
            }
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

    /// The tree the renderer would draw, as `(level, name, step/max)` —
    /// with the numbers brought up to date first, as the clock would have,
    /// and the padding after the text dropped.
    fn snap(layout: &Layout, root: &Arc<prodash::tree::Root>) -> Snapshot {
        layout.tick();
        let mut out = Vec::new();
        root.sorted_snapshot(&mut out);
        out.into_iter()
            .map(|(key, task)| {
                (
                    key.level(),
                    task.name.trim_end().to_string(),
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
        let layout = Layout::plain(10);
        let mut line = Line::over(root.add_child("headline"), "cactusbase", layout.clone());
        line.phase("cloning");
        assert_eq!(snap(&layout, &root), vec![(1, "cactusbase  cloning".into(), None)]);

        // gix renames the item it was handed as the fetch proceeds, and adds
        // its own children: none of that may grow the drawn tree.
        line.set_name("negotiate (round 1)".into());
        let mut remote = line.add_child("remote");
        remote.set_name("Counting objects".into());
        remote.init(Some(100), None);
        remote.set(40);
        assert_eq!(
            snap(&layout, &root),
            vec![(1, "cactusbase  Counting objects 40/100".into(), Some((40, Some(100))))]
        );
    }

    #[test]
    fn the_newest_phase_owns_the_line_and_stale_ones_cannot_write() {
        let root = prodash::tree::Root::new();
        let layout = Layout::plain(10);
        let mut line = Line::over(root.add_child("headline"), "cactusbase", layout.clone());
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
            snap(&layout, &root),
            vec![(1, "cactusbase  receiving pack 512/2048".into(), Some((512, Some(2048))))]
        );

        // When the phase ends the bar is cleared — numbers and all — and
        // the line falls back to the name gix last gave the handle it holds.
        line.set_name("receiving pack".into());
        drop(pack);
        assert_eq!(snap(&layout, &root), vec![(1, "cactusbase  receiving pack".into(), None)]);
        // ... and the still-live sideband child cannot resurrect its bar.
        remote.set(100);
        assert_eq!(snap(&layout, &root), vec![(1, "cactusbase  receiving pack".into(), None)]);
    }

    #[test]
    fn a_bounded_phase_is_not_displaced_by_its_unbounded_twin() {
        let root = prodash::tree::Root::new();
        let layout = Layout::plain(10);
        let mut line = Line::over(root.add_child("headline"), "cactusbase", layout.clone());
        let mut files = line.add_child("checkout");
        let mut written = line.add_child("writing");
        files.init(Some(1200), None);
        written.init(None, None);
        files.set(300);
        written.inc_by(4096);
        assert_eq!(
            snap(&layout, &root),
            vec![(1, "cactusbase  checking out 300/1200".into(), Some((300, Some(1200))))]
        );
    }

    #[test]
    fn a_second_ask_for_the_counter_does_not_reset_the_bar() {
        // gix's checkout counts through the counter it takes, not through
        // the handle, so the takeover is the only moment that may seed it —
        // and the clock is the only thing that can keep its numbers current.
        let root = prodash::tree::Root::new();
        let layout = Layout::plain(10);
        let mut line = Line::over(root.add_child("headline"), "cactusbase", layout.clone());
        let mut files = line.add_child("checkout");
        files.init(Some(1200), None);
        let counter = Count::counter(&files);
        counter.fetch_add(700, Ordering::Relaxed);
        assert_eq!(
            snap(&layout, &root),
            vec![(1, "cactusbase  checking out 700/1200".into(), Some((700, Some(1200))))]
        );
        drop(Count::counter(&files));
        assert_eq!(
            snap(&layout, &root),
            vec![(1, "cactusbase  checking out 700/1200".into(), Some((700, Some(1200))))]
        );
    }

    #[test]
    fn per_thread_workers_are_muted() {
        let root = prodash::tree::Root::new();
        let layout = Layout::plain(10);
        let mut line = Line::over(root.add_child("headline"), "cactusbase", layout.clone());
        let mut index = line.add_child("create index file");
        let mut resolving = index.add_child("Resolving");
        resolving.init(Some(9000), None);
        resolving.set(4500);
        // The phase names the line; its per-thread counters draw nothing.
        assert_eq!(snap(&layout, &root), vec![(1, "cactusbase  indexing pack".into(), None)]);
    }

    #[test]
    fn gix_chatter_is_dropped_but_failures_and_our_own_lines_survive() {
        let root = prodash::tree::Root::new();
        let layout = Layout::plain(10);
        let mut line = Line::over(root.add_child("headline"), "cactusbase", layout.clone());
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
            snap(&layout, &root),
            vec![(1, "cactusbase  receiving pack 2048/2048 bytes".into(), Some((2048, Some(2048))))]
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
        let layout = Layout::plain(12);
        let mut line = Line::counting(root.add_child("headline"), "flesh.tar.gz", layout.clone());
        line.phase("downloading");
        line.init(Some(4096), None);
        line.inc_by(1024);
        assert_eq!(
            snap(&layout, &root),
            vec![(1, "flesh.tar.gz  downloading 1024/4096".into(), Some((1024, Some(4096))))]
        );
        assert_eq!(Count::step(&line), 1024);
        assert_eq!(Progress::max(&line), Some(4096));
    }

    #[test]
    fn a_gix_line_draws_no_bar_of_its_own() {
        // gix counts a step or two on the handle it is given while walking
        // its own setup; "1 steps" is not a phase anyone is waiting on.
        let root = prodash::tree::Root::new();
        let layout = Layout::plain(10);
        let mut line = Line::over(root.add_child("headline"), "cactusbase", layout.clone());
        line.phase("cloning");
        line.init(Some(4), Some(prodash::unit::label("steps")));
        line.inc();
        assert_eq!(snap(&layout, &root), vec![(1, "cactusbase  cloning".into(), None)]);
    }

    #[test]
    fn the_bar_belongs_to_whichever_phase_is_moving() {
        let root = prodash::tree::Root::new();
        let layout = Layout::plain(10);
        let mut line = Line::over(root.add_child("headline"), "cactusbase", layout.clone());
        // gix opens both phases up front: the sideband child, and the pack
        // reader that will not see a byte until the server stops
        // compressing. Announcing a shape is not moving, so neither draws.
        let mut remote = line.add_child("remote");
        let mut pack = line.add_child("read pack");
        remote.init(Some(900), None);
        pack.init(None, Some(prodash::unit::label("bytes")));
        assert_eq!(snap(&layout, &root), vec![(1, "cactusbase  receiving pack".into(), None)]);

        // The server's progress is what is moving, so it gets the bar even
        // though the pack reader was created after it.
        remote.set_name("Compressing objects".into());
        remote.set(300);
        assert_eq!(
            snap(&layout, &root),
            vec![(1, "cactusbase  Compressing objects 300/900".into(), Some((300, Some(900))))]
        );

        // Then the bytes start, and the bar is theirs for good.
        pack.inc_by(4096);
        remote.set(900);
        assert_eq!(
            snap(&layout, &root),
            vec![(1, "cactusbase  receiving pack 4096 bytes".into(), Some((4096, None)))]
        );
    }

    #[test]
    fn a_streaming_phase_keeps_the_bar_and_hands_over_only_the_label() {
        let root = prodash::tree::Root::new();
        let layout = Layout::plain(10);
        let mut line = Line::over(root.add_child("headline"), "cactusbase", layout.clone());
        line.set_name("receiving pack".into());
        let mut pack = line.add_child("read pack");
        pack.init(None, Some(prodash::unit::label("bytes")));
        pack.inc_by(4096);

        // gix opens the indexing phase while the pack is still streaming
        // into it: the bytes are what is moving, so they keep the bar.
        let index = line.add_child("create index file");
        pack.inc_by(4096);
        assert_eq!(
            snap(&layout, &root),
            vec![(1, "cactusbase  receiving pack 8192 bytes".into(), Some((8192, None)))]
        );

        // Once the bytes stop, the label follows the work still running —
        // never back to "receiving pack", which the root still says.
        drop(pack);
        assert_eq!(snap(&layout, &root), vec![(1, "cactusbase  indexing pack".into(), None)]);
        drop(index);
        assert_eq!(snap(&layout, &root), vec![(1, "cactusbase  indexing pack".into(), None)]);
    }

    #[test]
    fn the_text_is_anchored_left_and_one_width_on_every_line() {
        let layout = Layout::plain(10);
        // The name is padded to its column, so every phase starts in the
        // same place; the numbers follow the phase directly; and the whole
        // text is padded to one width, so the bar prodash draws after it
        // starts in the same place on every line — and is the same width.
        let uv = layout.text("", "uv", "receiving pack", "4096 bytes");
        let gitoxide = layout.text("", "gitoxide", "checking out", "");
        assert!(uv.starts_with("uv          receiving pack 4096 bytes"), "{uv:?}");
        assert!(gitoxide.starts_with("gitoxide    checking out"), "{gitoxide:?}");
        assert_eq!(uv.find("receiving"), gitoxide.find("checking"));
        assert_eq!(uv.chars().count(), layout.text);
        assert_eq!(gitoxide.chars().count(), layout.text);
        // No phase yet: the name alone, still one width.
        assert_eq!(layout.text("", "uv", "", "").chars().count(), layout.text);

        // The headline sits one level up, so it is one column wider to put
        // its bar in the same place as the lines under it.
        let headline = layout.headline("fetch components", "0/2 components");
        assert!(headline.starts_with("fetch components  0/2 components"), "{headline:?}");
        assert_eq!(headline.chars().count(), layout.text + 1);
    }

    #[test]
    fn text_wider_than_its_column_is_clipped_not_wrapped() {
        let layout = Layout { text: 30, ..Layout::plain(10) };
        // The phase and numbers are clipped, with an ellipsis, to the room
        // the column leaves them.
        let clipped = layout.text("", "cactusbase", "Compressing objects", "12345/67890 objects [18%]");
        assert_eq!(clipped, "cactusbase  Compressing objec…");
        assert_eq!(clipped.chars().count(), 30);
        // A name wider than the column is never clipped — it eats into the
        // room its own phase has.
        let wide = "a-very-long-component-name";
        let clipped = layout.text("", wide, "checking out", "3/9");
        assert_eq!(clipped, format!("{wide}  c…"));
        // ... down to nothing, with the text still one width.
        let wider = "an-even-longer-component-name";
        assert_eq!(layout.text("", wider, "checking out", "3/9"), format!("{wider} "));
    }

    #[test]
    fn the_phase_is_quieter_than_the_name() {
        // prodash paints a task's whole name in one style, so the style has
        // to be ended inside the string, right after the name column.
        let colored = Layout { colors: true, ..Layout::plain(8) };
        let text = colored.text("", "uv", "receiving pack", "");
        assert!(text.starts_with("uv      \x1b[m  receiving pack"), "{text:?}");
        // The escape does not count toward the text column.
        assert_eq!(text.chars().count(), colored.text + 3);
        assert!(colored.headline("uv", "0/2 things").starts_with("uv      \x1b[m  0/2 things"));
        // With color off (a pipe, NO_COLOR), no escape is emitted at all —
        // a job log gets the columns and nothing else.
        let plain = Layout::plain(8);
        assert!(
            plain
                .text("", "uv", "receiving pack", "")
                .starts_with("uv        receiving pack")
        );
        assert!(plain.headline("uv", "0/2 things").starts_with("uv        0/2 things"));
    }

    #[test]
    fn the_bar_is_a_constant_share_of_the_terminal() {
        // 150 columns: a third for the bar, the rest for the text, less the
        // indent, prodash's empty numbers and the bar's own brackets.
        let wide = Layout::fit(24, 150, false);
        assert_eq!((wide.name, wide.text), (24, 150 - 50 - 7));
        // With color on, the three bytes of the escape count too.
        assert_eq!(Layout::fit(24, 150, true).text, wide.text - 3);
        // 45 columns: a third would be under the bar's floor.
        assert_eq!(Layout::fit(14, 45, false).text, 45 - MIN_BAR_COLUMN - 7);
        // Too narrow for anything: the text collapses rather than wraps.
        assert_eq!(Layout::fit(24, 20, false).text, 0);
    }

    #[test]
    fn numbers_read_as_prodash_would_have_drawn_them() {
        let bytes = prodash::unit::dynamic_and_mode(
            prodash::unit::Bytes,
            prodash::unit::display::Mode::with_throughput().and_percentage(),
        );
        assert_eq!(numbers(4096, Some(8192), Some(&bytes), None), "4.1kB/8.2kB [50%]");
        assert_eq!(
            numbers(4096, Some(8192), Some(&bytes), Some(1_200_000)),
            "4.1kB/8.2kB [50%] |1.2MB/s|"
        );
        assert_eq!(numbers(4096, None, Some(&bytes), Some(1_200_000)), "4.1kB |1.2MB/s|");
        let label = prodash::unit::label("objects");
        // A unit without a mode shows neither, whatever the rate.
        assert_eq!(numbers(40, Some(100), Some(&label), Some(7)), "40/100 objects");
        assert_eq!(numbers(40, Some(100), None, None), "40/100");
        assert_eq!(numbers(40, None, None, None), "40");
    }

    #[test]
    fn a_rate_is_sampled_a_second_at_a_time() {
        let t0 = std::time::Instant::now();
        let ms = std::time::Duration::from_millis;
        let mut rate = Rate::default();
        assert_eq!(rate.sample(0, t0), None);
        // Nothing to say until a full second has been seen.
        assert_eq!(rate.sample(500, t0 + ms(500)), None);
        assert_eq!(rate.sample(2000, t0 + ms(1000)), Some(2000));
        // The last rate stands until the next second is up.
        assert_eq!(rate.sample(2100, t0 + ms(1500)), Some(2000));
        assert_eq!(rate.sample(3000, t0 + ms(3000)), Some(500));
    }

    #[test]
    fn a_headline_counts_and_keeps_its_width() {
        let root = prodash::tree::Root::new();
        let layout = Layout::plain(16);
        let mut headline = Headline::over(
            root.add_child("headline"),
            "fetch components",
            12,
            "components",
            layout.clone(),
        );
        assert_eq!(
            snap(&layout, &root),
            vec![(1, "fetch components  0/12 components".into(), Some((0, Some(12))))]
        );
        for _ in 0..10 {
            headline.inc();
        }
        assert_eq!(
            snap(&layout, &root),
            vec![(1, "fetch components  10/12 components".into(), Some((10, Some(12))))]
        );
        // Padded to the column either way, so its bar never moves.
        let mut raw = Vec::new();
        root.sorted_snapshot(&mut raw);
        assert_eq!(raw[0].1.name.chars().count(), layout.text + 1);
        // Its children sit under it, one level down.
        let _child = headline.add_child("cactusbase");
        assert_eq!(snap(&layout, &root)[1], (2, "cactusbase".into(), None));
    }

    #[test]
    fn lines_hang_from_their_headline_and_the_last_one_gets_the_corner() {
        let root = prodash::tree::Root::new();
        let layout = Layout::plain(10);
        let mut top = Headline::over(root.add_child("top"), "fetch", 3, "components", layout.clone());
        let a = Line::over(top.add_child("a"), "a", layout.clone());
        let b = Line::over(top.add_child("b"), "b", layout.clone());
        let c = Line::over(top.add_child("c"), "c", layout.clone());
        c.phase("cloning");
        let names = |root: &Arc<prodash::tree::Root>| {
            snap(&layout, root).into_iter().map(|(_, name, _)| name).collect::<Vec<_>>()
        };
        assert_eq!(
            names(&root),
            vec!["fetch       0/3 components", "├─ a", "├─ b", "└─ c           cloning"]
        );
        // The branch is counted against the text column like the rest.
        let mut raw = Vec::new();
        root.sorted_snapshot(&mut raw);
        assert!(raw[1..].iter().all(|(_, task)| task.name.chars().count() == layout.text));

        // The corner passes to whichever line is last as soon as one goes.
        drop(c);
        top.inc();
        assert_eq!(names(&root), vec!["fetch       1/3 components", "├─ a", "└─ b"]);
        // ... and stays put as an earlier one goes.
        drop(a);
        top.inc();
        assert_eq!(names(&root), vec!["fetch       2/3 components", "└─ b"]);

        // A history line leaves its branch blank, so its name still lands
        // under the names of the lines that are running, and a line with
        // no headline over it hangs from nothing.
        b.succeeded("cloned");
        assert_eq!(
            messages(&root),
            vec![(MessageLevel::Success, "    b          ".into(), "cloned".into())]
        );
        let lone = Line::over(root.add_child("lone"), "lone", layout.clone());
        assert_eq!(names(&root).last().map(String::as_str), Some("lone"));
        lone.succeeded("done");
        assert_eq!(messages(&root)[1].1, " lone       ");
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
