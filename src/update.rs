//! Self-update (§17): the `autoupdate`, `update-url` and `mdb-url` knobs
//! (§5) and their defaults, the release manifest the update site publishes,
//! and installing a newer build.
//!
//! The knobs are maintenance knobs — they steer the binary itself, never a
//! job — so they are kept out of the knob snapshot frozen into
//! restart/build/test metadata.
//!
//! The site publishes `latest.json` = `{build, date, mdb_generation,
//! targets: {<target triple>: {path, sha256, size}}}`, where `path` names
//! the immutable `<target>/cactup-<build>` (the CDN caches every file on its
//! own, so a stable name could disagree with the manifest for minutes).
//! Every build lives at its own versioned path `$CACTUP_HOME/bin/cactup-<build>`
//! behind a `bin/cactup` symlink (the convention `freeze.rs` keeps for
//! `@CACTUP@`): installing one is a download beside the others, then an
//! atomic rename of a fresh symlink over `bin/cactup`, which is safe while
//! the old binary is running. The build it replaced is stamped
//! `cactup-<old>.retired`, and only `cactup update --prune` deletes retired
//! builds, never automatically: every submit script a restart chain writes
//! names its versioned binary, and a chain outliving its build would fail
//! to start its next restart.

use crate::build_info::{self, Stamp};
use crate::commands::Ctx;
use crate::database::Database;
use crate::lock::{self, LinkLock};
use crate::Res;
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, IsTerminal, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Where distribution builds look for new releases (`latest.json` and the
/// binaries) and where the documentation lives: the `update-url` default.
pub const DEFAULT_UPDATE_URL: &str = "https://max-morris.github.io/Cactup";

/// The git repository whose `mdb` branch carries the published machine
/// database: the `mdb-url` default.
pub const DEFAULT_MDB_URL: &str = "https://github.com/max-morris/Cactup.git";

/// What a distribution build does when a newer release exists (knob
/// `autoupdate`): install it and carry on in it, only say so, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoUpdate {
    Auto,
    Notify,
    Off,
}

impl AutoUpdate {
    const ALL: [Self; 3] = [Self::Auto, Self::Notify, Self::Off];

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Notify => "notify",
            Self::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Res<Self> {
        match Self::ALL.into_iter().find(|mode| mode.name() == s) {
            Some(mode) => Ok(mode),
            None => bail!("invalid autoupdate value \"{s}\" (valid: auto, notify, off)"),
        }
    }
}

/// Knob validator (§5): `autoupdate` stores the name form.
pub fn validate_autoupdate(value: &str) -> Res<String> {
    Ok(AutoUpdate::parse(value.trim())?.name().to_owned())
}

/// Knob validator (§5): `update-url` must be an https URL — what it serves
/// is installed and run without asking, so a plain-http site would let
/// anyone on the path hand out code. Plain http is accepted only for a
/// loopback test server (`127.0.0.1`, `localhost`, `[::1]`), the rule
/// `cactup-init.sh` applies to `CACTUP_UPDATE_ROOT`. Stored without a
/// trailing `/`, since paths are appended to it.
pub fn validate_update_url(value: &str) -> Res<String> {
    let url = value.trim().trim_end_matches('/');
    if url.contains(char::is_whitespace) {
        bail!("invalid update-url \"{value}\": expected an https:// URL");
    }
    if url.strip_prefix("https://").is_some_and(|rest| !rest.is_empty()) {
        return Ok(url.to_owned());
    }
    match url.strip_prefix("http://") {
        Some(rest) if is_loopback(url_host(rest)) => Ok(url.to_owned()),
        Some(_) => bail!(
            "invalid update-url \"{value}\": https is required (plain http:// is accepted only for a \
             loopback test server: 127.0.0.1, localhost or [::1])"
        ),
        None => bail!("invalid update-url \"{value}\": expected an https:// URL"),
    }
}

/// The host of a URL with its scheme removed: the authority up to the path,
/// without userinfo or port (an IPv6 literal keeps its brackets).
fn url_host(rest: &str) -> &str {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host_port.starts_with('[') {
        return host_port.find(']').map_or(host_port, |end| &host_port[..=end]);
    }
    host_port.split(':').next().unwrap_or("")
}

/// Is `host` (from [`url_host`]) the loopback interface?
fn is_loopback(host: &str) -> bool {
    ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|lo| host.eq_ignore_ascii_case(lo))
}

/// Knob validator (§5): `mdb-url` is anything git can fetch from (an
/// https URL, an ssh remote, a local path), so it is only required to be
/// non-empty.
pub fn validate_mdb_url(value: &str) -> Res<String> {
    let url = value.trim();
    if url.is_empty() {
        bail!("mdb-url cannot be empty (`cactup knob delete mdb-url` restores the default)");
    }
    Ok(url.to_owned())
}

/// The effective `autoupdate` setting, read leniently: a stored value that
/// no longer parses means the default, `auto`, rather than failing whatever
/// command is running.
pub fn autoupdate(db: &Database) -> AutoUpdate {
    db.knob_or_default("autoupdate")
        .and_then(|v| AutoUpdate::parse(&v).ok())
        .unwrap_or(AutoUpdate::Auto)
}

/// The effective `update-url`, without a trailing `/`.
pub fn update_url(db: &Database) -> String {
    let url = db
        .knob_or_default("update-url")
        .unwrap_or_else(|| DEFAULT_UPDATE_URL.to_owned());
    url.trim_end_matches('/').to_owned()
}

/// How often the automatic check asks the update site: at most once a day
/// per `$CACTUP_HOME`, whatever the outcome.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a retired build is kept before `cactup update --prune` removes it.
pub const RETIRED_KEEP: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Bounds on asking the update site for `latest.json`: an unreachable site
/// must cost an interactive command seconds, not a TCP timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const LATEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a freshly downloaded binary gets to answer `--version`.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Set in the environment of a cactup that an update has just started, so
/// it neither checks again nor loops.
const UPDATED_ENV: &str = "CACTUP_UPDATED";

/// The name of the update lock in the bin directory.
const UPDATE_LOCK: &str = ".update.lock";

/// The release manifest, `<update-url>/latest.json`. Read leniently: keys
/// a later site adds are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Latest {
    /// The build id (abbreviated commit hash) of the published release.
    pub build: String,
    /// Its committer date, strict ISO 8601.
    pub date: String,
    /// The MDB generation it reads.
    pub mdb_generation: u32,
    /// One binary per target triple.
    #[serde(default)]
    pub targets: BTreeMap<String, TargetEntry>,
}

/// One published binary.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TargetEntry {
    /// Relative to the update URL: `<target>/cactup-<build>`.
    pub path: String,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: String,
    /// Its size in bytes.
    pub size: u64,
}

/// What the published release means for this binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The published build is this one.
    UpToDate,
    /// The published build is not newer than this one (a rollback, a CDN
    /// still serving the previous manifest, or a date that does not parse).
    ServerOlder,
    /// Newer, but nothing is published for this target.
    NoTarget,
    /// Newer, and this is the binary to install.
    Newer(TargetEntry),
}

/// Is `s` a build id: 7-40 lowercase hex digits?
fn is_build_id(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `cactup-<build>`: the file a build lives at in the bin directory.
fn binary_name(build: &str) -> String {
    format!("cactup-{build}")
}

/// The build a `cactup-<build>` file name names.
fn build_of(name: &str) -> Option<&str> {
    name.strip_prefix("cactup-").filter(|id| is_build_id(id))
}

/// Do two build ids name the same commit? git abbreviates to at least seven
/// digits but may use more, so one may be a prefix of the other.
fn same_build(a: &str, b: &str) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn parse_date(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(s.trim()).ok()
}

/// Decide what the published release `latest` means for the running build
/// `me` on `target`. A different build is newer only when its committer
/// date is strictly later — builds are ordered by date, never by id, and a
/// date that does not parse never counts as newer. A malformed manifest
/// (a build id that is not hex, a path that leaves the site, a checksum that
/// is not one) is an error.
pub fn decide(latest: &Latest, me: &Stamp, target: &str) -> Res<Decision> {
    if !is_build_id(&latest.build) {
        bail!("the update site publishes an invalid build id \"{}\"", latest.build);
    }
    if same_build(&latest.build, me.id) {
        return Ok(Decision::UpToDate);
    }
    let newer = match (parse_date(&latest.date), parse_date(me.date)) {
        (Some(published), Some(mine)) => published > mine,
        _ => false,
    };
    if !newer {
        return Ok(Decision::ServerOlder);
    }
    let Some(entry) = latest.targets.get(target) else {
        return Ok(Decision::NoTarget);
    };
    let path = Path::new(&entry.path);
    if entry.path.is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("the update site publishes an invalid path \"{}\" for {target}", entry.path);
    }
    if entry.sha256.len() != 64 || !entry.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("the update site publishes an invalid checksum \"{}\" for {target}", entry.sha256);
    }
    Ok(Decision::Newer(entry.clone()))
}

/// Fetch `<base>/latest.json`: its own client, 5 s to connect and 10 s in
/// all, so an unreachable site cannot hold up a command for long.
pub fn fetch_latest(base: &str) -> Res<Latest> {
    let url = format!("{}/latest.json", base.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .user_agent(build_info::USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(LATEST_TIMEOUT)
        .build()
        .context("Failed to build the HTTP client")?;
    let response = client
        .get(&url)
        .send()
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("Failed to fetch {url}"))?;
    // A release manifest is a few hundred bytes; anything past a MiB is not one.
    let mut body = Vec::new();
    response
        .take(1 << 20)
        .read_to_end(&mut body)
        .with_context(|| format!("Failed to read {url}"))?;
    serde_json::from_slice(&body).with_context(|| format!("{url} is not a valid release manifest"))
}

/// [`fetch_latest`] under a progress line, and interruptible: the request
/// runs on a helper thread, since a blocking HTTP call cannot poll the
/// interrupt flag itself, and an interrupt abandons it.
pub fn check(base: &str) -> Res<Latest> {
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    const NAME: &str = "cactup";
    let layout = crate::progress::Layout::for_names([NAME]);
    let line = crate::progress::Line::over(progress.add_child(NAME), NAME, layout);
    line.phase("checking for updates");

    let (tx, rx) = std::sync::mpsc::channel();
    let base = base.to_owned();
    std::thread::spawn(move || {
        let _ = tx.send(fetch_latest(&base));
    });
    let result = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => break result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if gix::interrupt::is_triggered() {
                    break Err(anyhow!("interrupted"));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break Err(anyhow!("the update check stopped unexpectedly"));
            }
        }
    };
    // Nothing to leave in the scrollback: the caller says what it found.
    drop(line);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    result
}

/// Whether the running binary can replace itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installability {
    Updatable,
    /// Why not, for the user.
    NotUpdatable(String),
}

/// Can the binary at `exe` be updated in `bin_dir`? Only when it lives
/// there, as `cactup` or `cactup-<build>` — where `cactup-init.sh` and
/// earlier updates put it. A binary anywhere else (a package manager's, a
/// copy in a project, a cargo build) is left for whoever put it there.
/// Both paths are canonicalized: `/proc/self/exe` already is, but
/// `$CACTUP_HOME` may run through a symlink.
pub fn installability(exe: &Path, bin_dir: &Path) -> Installability {
    let exe = match exe.canonicalize() {
        Ok(exe) => exe,
        Err(e) => {
            return Installability::NotUpdatable(format!(
                "cannot locate the running cactup ({}: {e})",
                exe.display()
            ));
        }
    };
    let not_ours = || {
        Installability::NotUpdatable(format!(
            "it runs from {}, not from {}, where cactup-init.sh installs; update it the way it \
             was installed",
            exe.display(),
            bin_dir.display()
        ))
    };
    let Ok(bin) = bin_dir.canonicalize() else { return not_ours() };
    let name_ok = exe
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "cactup" || build_of(n).is_some());
    if exe.parent() == Some(bin.as_path()) && name_ok { Installability::Updatable } else { not_ours() }
}

/// How an [`apply`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The new build is installed at this path and `bin/cactup` names it.
    Installed(PathBuf),
    /// Another cactup installed this build first.
    AlreadyInstalled,
    /// Another cactup is installing an update right now.
    Busy,
    /// The published file is missing or does not match `latest.json` yet:
    /// a release is still propagating through the CDN. Retry later.
    NotYet,
    /// The bin directory cannot be written; why, for the user.
    NotUpdatable(String),
}

/// Is `e` a permission problem (EACCES, EPERM, EROFS) anywhere in its chain?
fn is_permission_error(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(io.kind(), ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem)
        })
    })
}

fn cannot_write(bin_dir: &Path, e: &anyhow::Error) -> Applied {
    Applied::NotUpdatable(format!(
        "{} is not writable ({}); ask whoever installed cactup there to update it",
        bin_dir.display(),
        e.root_cause()
    ))
}

/// Install `entry` of the release `latest` from the update site `base` into
/// `bin_dir`, replacing the running build `me`: download beside the other
/// builds, verify size and SHA-256 before anything runs it, check that it
/// answers `--version` with its build id, move it into place as
/// `cactup-<build>`, point `bin/cactup` at it, and stamp the build it
/// replaced as retired. Under `bin/.update.lock`; progress on one line.
pub fn apply(
    base: &str,
    latest: &Latest,
    entry: &TargetEntry,
    bin_dir: &Path,
    me: &Stamp,
) -> Res<Applied> {
    let (progress, renderer) =
        crate::manifest::setup_prodash_with(Some(crate::manifest::progress_level_filter(1)), true);
    const NAME: &str = "cactup";
    let layout = crate::progress::Layout::for_names([NAME]);
    let mut line = crate::progress::Line::counting(progress.add_child(NAME), NAME, layout);
    let result = apply_with(base, latest, entry, bin_dir, me, &mut line);
    drop(line);
    renderer.shutdown_and_wait();
    result
}

/// [`apply`], reporting on `line`.
fn apply_with(
    base: &str,
    latest: &Latest,
    entry: &TargetEntry,
    bin_dir: &Path,
    me: &Stamp,
    line: &mut crate::progress::Line,
) -> Res<Applied> {
    // §2.3: link()-based, never flock; the heartbeat keeps a slow download
    // from reading as a stale lock on another host.
    let _lock = match LinkLock::try_acquire(&bin_dir.join(UPDATE_LOCK)) {
        Ok(Some(lock)) => lock.with_heartbeat(),
        Ok(None) => return Ok(Applied::Busy),
        Err(e) if is_permission_error(&e) => return Ok(cannot_write(bin_dir, &e)),
        Err(e) => return Err(e),
    };

    let name = binary_name(&latest.build);
    let versioned = bin_dir.join(&name);
    let link = bin_dir.join("cactup");
    if fs::read_link(&link).is_ok_and(|target| target == Path::new(&name)) && versioned.is_file() {
        return Ok(Applied::AlreadyInstalled);
    }

    let mut download = match tempfile::Builder::new().prefix(".cactup-download.").tempfile_in(bin_dir) {
        Ok(file) => file,
        Err(e) => {
            let e = anyhow::Error::new(e);
            if is_permission_error(&e) {
                return Ok(cannot_write(bin_dir, &e));
            }
            return Err(e.context(format!("Failed to create a temp file in {}", bin_dir.display())));
        }
    };
    line.phase(format!("downloading {}", latest.build));
    let url = format!("{}/{}", base.trim_end_matches('/'), entry.path);
    let size = match crate::fetch::download::download_to(
        crate::fetch::download::client(),
        &url,
        download.as_file_mut(),
        line,
    ) {
        Ok(size) => size,
        Err(e) if is_not_found(&e) => return Ok(Applied::NotYet),
        Err(e) => return Err(e),
    };
    // Verified before anything executes it. A mismatch is the CDN serving a
    // manifest and a binary from different releases, not corruption worth an
    // error: the next check finds them agreeing.
    if size != entry.size || !sha256_hex(download.path())?.eq_ignore_ascii_case(&entry.sha256) {
        return Ok(Applied::NotYet);
    }

    line.phase(format!("checking {}", latest.build));
    // Closing our write handle first: exec of a file open for writing
    // fails with ETXTBSY.
    let download = download.into_temp_path();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&download, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("Failed to make {} executable", download.display()))?;
    }
    answers_version(&download, &latest.build)
        .with_context(|| format!("The downloaded cactup {} does not run here", latest.build))?;

    // Which build `bin/cactup` names now, and so retires: the symlink's
    // target, or — a plain file, from an older install — this very binary,
    // whose versioned copy `freeze.rs` may have made.
    let previous = match fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => fs::read_link(&link)
            .ok()
            .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned())),
        Ok(_) => Some(binary_name(me.id)),
        Err(_) => None,
    };

    download.persist(&versioned).with_context(|| {
        format!("Failed to move the new cactup into place at {}", versioned.display())
    })?;
    // A fresh symlink renamed over `bin/cactup`: atomic, and harmless to the
    // old binary, which keeps running from its own inode.
    let fresh = tempfile::Builder::new()
        .prefix(".cactup-link.")
        .make_in(bin_dir, |path| std::os::unix::fs::symlink(&name, path))
        .with_context(|| format!("Failed to create a symlink in {}", bin_dir.display()))?;
    fresh
        .persist(&link)
        .with_context(|| format!("Failed to point {} at {name}", link.display()))?;

    // Best-effort bookkeeping for `--prune`: the update itself is done, and a
    // missing stamp only means a build is kept longer.
    let _ = fs::remove_file(bin_dir.join(format!("{name}.retired")));
    if let Some(previous) = previous.filter(|p| *p != name && build_of(p).is_some()) {
        let _ = lock::write_stamp(
            &bin_dir.join(format!("{previous}.retired")),
            format!("replaced by {name}\n").as_bytes(),
        );
    }
    line.succeeded(format!("updated to {} ({})", latest.build, short_date(&latest.date)));
    Ok(Applied::Installed(versioned))
}

/// Is `e` an HTTP 404 anywhere in its chain?
fn is_not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
            .is_some_and(|status| status == reqwest::StatusCode::NOT_FOUND)
    })
}

/// The lowercase hex SHA-256 of the file at `path`.
fn sha256_hex(path: &Path) -> Res<String> {
    let mut file = fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut context = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        if n == 0 {
            break;
        }
        context.update(&buf[..n]);
    }
    Ok(context.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

/// Run `<path> --version` (at most [`VERSION_TIMEOUT`], interrupt-aware)
/// and require it to succeed and name `build`.
fn answers_version(path: &Path, build: &str) -> Res<()> {
    use std::process::{Command, Stdio};
    let spawn = || {
        Command::new(path)
            .arg("--version")
            .env(UPDATED_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    };
    // Another thread forking at the moment the download was still open for
    // writing leaves a brief ETXTBSY window: retry it.
    let mut attempts = 0;
    let mut child = loop {
        match spawn() {
            Err(e) if e.kind() == ErrorKind::ExecutableFileBusy && attempts < 20 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            other => break other.with_context(|| format!("Failed to run {}", path.display()))?,
        }
    };
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("Failed to wait for --version")? {
            break status;
        }
        if gix::interrupt::is_triggered() || started.elapsed() > VERSION_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            if gix::interrupt::is_triggered() {
                bail!("interrupted");
            }
            bail!("`--version` did not answer within {} s", VERSION_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if !status.success() {
        bail!("`--version` failed ({status})");
    }
    if !stdout.contains(build) {
        bail!("`--version` does not name build {build}: {}", stdout.trim());
    }
    Ok(())
}

/// Remove the builds in `bin_dir` retired more than [`RETIRED_KEEP`] ago,
/// with their `.retired` stamps, returning the paths removed. Never the
/// build `bin/cactup` names or the running one (`exe`), and never a build
/// without a stamp. Under the update lock, so it cannot race an install.
pub fn prune_binaries(bin_dir: &Path, exe: &Path) -> Res<Vec<PathBuf>> {
    if !bin_dir.is_dir() {
        return Ok(Vec::new());
    }
    let Some(_lock) = LinkLock::try_acquire(&bin_dir.join(UPDATE_LOCK))? else {
        bail!("another cactup is updating in {} right now; try again in a moment", bin_dir.display());
    };
    let current = fs::read_link(bin_dir.join("cactup"))
        .ok()
        .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned()));
    let running = exe.canonicalize().ok();

    let mut removed = Vec::new();
    let entries =
        fs::read_dir(bin_dir).with_context(|| format!("Failed to list {}", bin_dir.display()))?;
    for entry in entries {
        if gix::interrupt::is_triggered() {
            bail!("interrupted");
        }
        let entry = entry.with_context(|| format!("Failed to list {}", bin_dir.display()))?;
        let file_name = entry.file_name();
        let Some(binary) = file_name.to_str().and_then(|n| n.strip_suffix(".retired")) else { continue };
        if build_of(binary).is_none() || current.as_deref() == Some(binary) {
            continue;
        }
        let path = bin_dir.join(binary);
        if running.is_some() && path.canonicalize().ok() == running {
            continue;
        }
        let stamp = entry.path();
        if lock::mtime_age(&stamp).is_none_or(|age| age < RETIRED_KEEP) {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => removed.push(path),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("Failed to remove {}", path.display())),
        }
        let _ = fs::remove_file(&stamp);
    }
    removed.sort();
    Ok(removed)
}

/// Replace this process with the cactup at `path`, run with `args` and
/// [`UPDATED_ENV`] set. Returns only if the exec failed.
pub fn exec_updated(path: &Path, args: &[OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(path).args(args).env(UPDATED_ENV, "1").exec()
}

static JUST_UPDATED: AtomicBool = AtomicBool::new(false);

/// Record whether an update started this process ([`UPDATED_ENV`]) and
/// remove the variable, so nothing this cactup runs inherits it. Call first
/// thing in `main`, before any thread exists.
pub fn take_updated_marker() {
    if std::env::var_os(UPDATED_ENV).is_some() {
        JUST_UPDATED.store(true, Ordering::Relaxed);
        // SAFETY: called at the top of main, while the process is still
        // single-threaded, so nothing can be reading the environment.
        unsafe { std::env::remove_var(UPDATED_ENV) };
    }
}

/// Did an update start this process?
pub fn just_updated() -> bool {
    JUST_UPDATED.load(Ordering::Relaxed)
}

/// `YYYY-MM-DD` of an ISO 8601 date.
pub fn short_date(date: &str) -> &str {
    date.get(..10).unwrap_or(date)
}

/// The automatic check before an interactive command. Never fails and stays
/// silent unless there is something to say. Nothing happens for a dev build,
/// when stderr is not a terminal (scripts, jobs, pipes), in a process an
/// update just started, within a day of the last check, or with
/// `autoupdate = off`. Otherwise it asks the update site (the day's check is
/// spent whether or not that works; a failure is silent) and, when a newer
/// build exists, either says so (`notify`) or installs it and re-runs this
/// same command in it (`auto`).
pub fn maybe_auto_update(ctx: &Ctx) {
    let Some(me) = build_info::DIST.filter(|_| build_info::is_dist()) else { return };
    if !std::io::stderr().is_terminal() || just_updated() {
        return;
    }
    let stamp = crate::CACTUP_ROOT.join("update-check");
    if lock::mtime_age(&stamp).is_some_and(|age| age < CHECK_INTERVAL) {
        return;
    }
    let Ok(db) = ctx.db.read() else { return };
    let mode = autoupdate(&db);
    if mode == AutoUpdate::Off {
        return;
    }
    let base = update_url(&db);
    drop(db);

    let latest = check(&base);
    if gix::interrupt::is_triggered() {
        return;
    }
    let _ = fs::create_dir_all(&*crate::CACTUP_ROOT);
    let seen = latest.as_ref().map(|l| format!("{}\n", l.build)).unwrap_or_default();
    let _ = lock::write_stamp(&stamp, seen.as_bytes());

    let Ok(latest) = latest else { return };
    let Ok(Decision::Newer(entry)) = decide(&latest, &me, build_info::TARGET) else { return };
    let available = format!("cactup {} is available (you have {})", latest.build, me.id);
    if mode == AutoUpdate::Notify {
        eprintln!("{}", format!("{available}; run `cactup update`").yellow());
        return;
    }

    let bin_dir = crate::CACTUP_ROOT.join("bin");
    let not_updatable = |reason: &str| {
        eprintln!(
            "{}",
            format!("{available}, but cannot be installed automatically: {reason}").yellow()
        );
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return not_updatable(&format!("cannot locate the running cactup ({e})")),
    };
    if let Installability::NotUpdatable(reason) = installability(&exe, &bin_dir) {
        return not_updatable(&reason);
    }
    let path = match apply(&base, &latest, &entry, &bin_dir, &me) {
        Ok(Applied::Installed(path)) => path,
        // Someone else just installed it: carry on in it all the same.
        Ok(Applied::AlreadyInstalled) => bin_dir.join(binary_name(&latest.build)),
        // Someone else is installing it, or it is still propagating: the
        // next check, or `cactup update`, gets it.
        Ok(Applied::Busy | Applied::NotYet) => return,
        Ok(Applied::NotUpdatable(reason)) => return not_updatable(&reason),
        Err(e) => {
            if !gix::interrupt::is_triggered() {
                eprintln!("{}", format!("Warning: could not update cactup: {e:#}").yellow());
            }
            return;
        }
    };
    // The renderer and the update lock are gone by now (`apply` returned);
    // run this same command in the new build.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let e = exec_updated(&path, &args);
    eprintln!(
        "{}",
        format!(
            "Warning: could not start the updated cactup at {}: {e}; carrying on with this one",
            path.display()
        )
        .yellow()
    );
}

/// Bring the machine database up to date now, ignoring the sync throttle.
pub(crate) fn force_mdb_sync(ctx: &Ctx) -> Res<()> {
    let root = crate::CACTUP_ROOT.join("mdb");
    crate::mdb::sync::system_root(&root, &ctx.db, crate::mdb::sync::Mode::Force).map(|_| ())
}

/// The loud notice that the published machine database has moved on to a
/// newer generation than this binary reads, if the last sync saw one.
pub(crate) fn mdb_generation_notice() -> Option<String> {
    crate::mdb::sync::newer_generation_notice(&crate::CACTUP_ROOT.join("mdb"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autoupdate_values() {
        assert_eq!(validate_autoupdate("notify").unwrap(), "notify");
        assert_eq!(validate_autoupdate(" off ").unwrap(), "off");
        for mode in AutoUpdate::ALL {
            assert_eq!(AutoUpdate::parse(mode.name()).unwrap(), mode);
        }
        let err = validate_autoupdate("sometimes").unwrap_err().to_string();
        assert!(err.contains("auto, notify, off"), "{err}");
    }

    #[test]
    fn update_url_values() {
        assert_eq!(
            validate_update_url("https://example.org/cactup/").unwrap(),
            "https://example.org/cactup"
        );
        assert_eq!(validate_update_url("http://127.0.0.1:8080//").unwrap(), "http://127.0.0.1:8080");
        // Plain http only for a loopback test server.
        for ok in
            ["http://localhost", "http://LOCALHOST:9/site", "http://[::1]:8000/x", "http://u@127.0.0.1"]
        {
            assert_eq!(validate_update_url(ok).unwrap(), ok);
        }
        for bad in [
            "",
            "example.org",
            "ftp://example.org",
            "https://",
            "https://a b",
            "http://",
            "http://example.org",
            "http://127.0.0.2:8080",
            "http://localhost.example.org",
            "http://127.0.0.1.example.org/x",
            "http://127.0.0.1@example.org",
            "http://[::2]",
        ] {
            assert!(validate_update_url(bad).is_err(), "{bad:?} accepted");
        }
        let err = validate_update_url("http://mirror.example.org/cactup").unwrap_err().to_string();
        assert!(err.contains("https is required") && err.contains("loopback"), "{err}");
        assert_eq!(validate_update_url(DEFAULT_UPDATE_URL).unwrap(), DEFAULT_UPDATE_URL);
    }

    #[test]
    fn mdb_url_values() {
        assert_eq!(validate_mdb_url(" /srv/git/cactup.git ").unwrap(), "/srv/git/cactup.git");
        assert_eq!(
            validate_mdb_url("git@github.com:max-morris/Cactup.git").unwrap(),
            "git@github.com:max-morris/Cactup.git"
        );
        assert!(validate_mdb_url("  ").is_err());
    }

    #[test]
    fn defaults_and_lenient_read() {
        let mut db = Database::new();
        assert_eq!(autoupdate(&db), AutoUpdate::Auto);
        assert_eq!(db.knob_or_default("update-url").as_deref(), Some(DEFAULT_UPDATE_URL));
        assert_eq!(db.knob_or_default("mdb-url").as_deref(), Some(DEFAULT_MDB_URL));

        db.set_knob("autoupdate", "notify".to_owned());
        assert_eq!(autoupdate(&db), AutoUpdate::Notify);
        // A value that no longer parses falls back to the default.
        db.set_knob("autoupdate", "garbage".to_owned());
        assert_eq!(autoupdate(&db), AutoUpdate::Auto);
    }

    #[test]
    fn maintenance_knobs_stay_out_of_the_snapshot() {
        let mut db = Database::new();
        db.set_knob("autoupdate", "off".to_owned());
        db.set_knob("update-url", "https://example.org".to_owned());
        db.set_knob("allocation", "hpc_xxx".to_owned());
        let snapshot = db.knob_snapshot();
        for knob in ["autoupdate", "update-url", "mdb-url"] {
            assert!(crate::database::knob_spec(knob).is_some(), "{knob} is a standard knob");
            assert!(!snapshot.contains_key(knob), "{knob} leaked into the snapshot");
        }
        assert_eq!(snapshot.get("allocation").map(String::as_str), Some("hpc_xxx"));
    }

    // ---- self-update ----

    use crate::fetch::download::test_server;
    use std::os::unix::fs::PermissionsExt;

    const TARGET: &str = "x86_64-unknown-linux-musl";
    const ME: Stamp = Stamp { id: "aaaaaaa", date: "2026-09-20T12:00:00-05:00" };

    fn entry_for(path: &str, body: &[u8]) -> TargetEntry {
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp.path(), body).unwrap();
        TargetEntry {
            path: path.to_owned(),
            sha256: sha256_hex(temp.path()).unwrap(),
            size: body.len() as u64,
        }
    }

    fn latest(build: &str, date: &str, entry: Option<TargetEntry>) -> Latest {
        Latest {
            build: build.to_owned(),
            date: date.to_owned(),
            mdb_generation: 1,
            targets: entry.into_iter().map(|e| (TARGET.to_owned(), e)).collect(),
        }
    }

    /// A stand-in for a published cactup: a script answering `--version`
    /// the way the real binary does.
    fn payload(build: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho \"cactup 0.1.0 ({build} 2026-09-24, mdb generation 1)\"\n").into_bytes()
    }

    fn test_line() -> crate::progress::Line {
        crate::progress::Line::counting(
            prodash::tree::Root::new().add_child("cactup"),
            "cactup",
            crate::progress::Layout::for_names(["cactup"]),
        )
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_manifest_parses_leniently() {
        let json = r#"{"build":"bbbbbbb","date":"2026-09-24T10:00:00+00:00","mdb_generation":2,
            "future_key":true,
            "targets":{"x86_64-unknown-linux-musl":{"path":"x86_64-unknown-linux-musl/cactup-bbbbbbb",
            "sha256":"00","size":3,"also_new":1}}}"#;
        let latest: Latest = serde_json::from_str(json).unwrap();
        assert_eq!(latest.mdb_generation, 2);
        assert_eq!(latest.targets[TARGET].size, 3);
    }

    #[test]
    fn decide_orders_builds_by_date() {
        let entry = entry_for("x86_64-unknown-linux-musl/cactup-bbbbbbb", b"x");
        let newer = latest("bbbbbbb", "2026-09-24T00:00:00+00:00", Some(entry.clone()));
        assert_eq!(decide(&newer, &ME, TARGET).unwrap(), Decision::Newer(entry.clone()));

        // The same build, also when one id is a longer abbreviation.
        let same = latest("aaaaaaa", "2026-09-24T00:00:00+00:00", Some(entry.clone()));
        assert_eq!(decide(&same, &ME, TARGET).unwrap(), Decision::UpToDate);
        let longer = latest("aaaaaaa1", "2026-09-24T00:00:00+00:00", Some(entry.clone()));
        assert_eq!(decide(&longer, &ME, TARGET).unwrap(), Decision::UpToDate);

        // Older, the very same instant in another zone, and unparseable:
        // never newer.
        let older = latest("bbbbbbb", "2026-09-19T00:00:00+00:00", Some(entry.clone()));
        assert_eq!(decide(&older, &ME, TARGET).unwrap(), Decision::ServerOlder);
        let equal = latest("bbbbbbb", "2026-09-20T17:00:00+00:00", Some(entry.clone()));
        assert_eq!(decide(&equal, &ME, TARGET).unwrap(), Decision::ServerOlder);
        let garbled = latest("bbbbbbb", "yesterday", Some(entry.clone()));
        assert_eq!(decide(&garbled, &ME, TARGET).unwrap(), Decision::ServerOlder);
        let bad_me = Stamp { id: "aaaaaaa", date: "not a date" };
        assert_eq!(decide(&newer, &bad_me, TARGET).unwrap(), Decision::ServerOlder);

        // Newer, but not for this target.
        let elsewhere = latest("bbbbbbb", "2026-09-24T00:00:00+00:00", None);
        assert_eq!(decide(&elsewhere, &ME, TARGET).unwrap(), Decision::NoTarget);
    }

    #[test]
    fn decide_rejects_a_malformed_manifest() {
        let date = "2026-09-24T00:00:00+00:00";
        for build in ["BBBBBBB", "bbbbbb", "bbbbbbb/../x", "", &"b".repeat(41)] {
            let bad = latest(build, date, Some(entry_for("t/cactup-x", b"x")));
            assert!(decide(&bad, &ME, TARGET).is_err(), "build {build:?} accepted");
        }
        for path in ["/etc/passwd", "../cactup", "t/../../cactup", "", "./t/cactup"] {
            let bad = latest("bbbbbbb", date, Some(entry_for(path, b"x")));
            let err = decide(&bad, &ME, TARGET).unwrap_err().to_string();
            assert!(err.contains("invalid path"), "path {path:?}: {err}");
        }
        let mut entry = entry_for("t/cactup-bbbbbbb", b"x");
        entry.sha256 = "abc".to_owned();
        let bad = latest("bbbbbbb", date, Some(entry));
        assert!(decide(&bad, &ME, TARGET).unwrap_err().to_string().contains("invalid checksum"));
    }

    #[test]
    fn fetch_latest_reads_the_manifest_from_the_site() {
        let json =
            br#"{"build":"bbbbbbb","date":"2026-09-24T00:00:00+00:00","mdb_generation":1,"targets":{}}"#;
        let (base, server) = test_server::serve(vec![("/latest.json", json.to_vec())], 1);
        let latest = fetch_latest(&format!("{base}/")).unwrap();
        assert_eq!(latest.build, "bbbbbbb");
        assert!(server.join().unwrap()[0].starts_with("GET /latest.json "));

        let (base, server) = test_server::serve(vec![], 1);
        assert!(fetch_latest(&base).is_err(), "a 404 is an error");
        server.join().unwrap();
    }

    #[test]
    fn installability_follows_symlinks_to_the_bin_dir() {
        let real = tempfile::tempdir().unwrap();
        let bin = real.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for name in ["cactup-aaaaaaa", "cactup-tool", "other"] {
            fs::write(bin.join(name), b"").unwrap();
        }
        std::os::unix::fs::symlink("cactup-aaaaaaa", bin.join("cactup")).unwrap();
        // $CACTUP_HOME reached through a symlink, as on clusters where the
        // home directory is one.
        let alias_dir = tempfile::tempdir().unwrap();
        let home = alias_dir.path().join("home");
        std::os::unix::fs::symlink(real.path(), &home).unwrap();

        let via = home.join("bin");
        assert_eq!(installability(&via.join("cactup"), &via), Installability::Updatable);
        assert_eq!(installability(&bin.join("cactup-aaaaaaa"), &via), Installability::Updatable);
        for name in ["cactup-tool", "other"] {
            assert!(
                matches!(installability(&bin.join(name), &via), Installability::NotUpdatable(_)),
                "{name} is not a cactup build"
            );
        }
        let stray = real.path().join("cactup");
        fs::write(&stray, b"").unwrap();
        let Installability::NotUpdatable(reason) = installability(&stray, &via) else {
            panic!("a binary outside the bin dir is updatable")
        };
        assert!(reason.contains("update it the way it was installed"), "{reason}");
        assert!(matches!(
            installability(&bin.join("cactup"), &real.path().join("missing")),
            Installability::NotUpdatable(_)
        ));
    }

    #[test]
    fn apply_replaces_a_plain_binary_with_a_versioned_one_and_a_link() {
        let bin = tempfile::tempdir().unwrap();
        fs::write(bin.path().join("cactup"), b"the old plain binary").unwrap();
        // The versioned copy `freeze.rs` made of it.
        fs::write(bin.path().join("cactup-aaaaaaa"), b"the old plain binary").unwrap();

        let body = payload("bbbbbbb");
        let entry = entry_for("x86_64-unknown-linux-musl/cactup-bbbbbbb", &body);
        let release = latest("bbbbbbb", "2026-09-24T00:00:00+00:00", Some(entry.clone()));
        let (base, server) =
            test_server::serve(vec![("/x86_64-unknown-linux-musl/cactup-bbbbbbb", body.clone())], 1);

        let applied = apply_with(&base, &release, &entry, bin.path(), &ME, &mut test_line()).unwrap();
        server.join().unwrap();
        let installed = bin.path().join("cactup-bbbbbbb");
        assert_eq!(applied, Applied::Installed(installed.clone()));
        assert_eq!(fs::read(&installed).unwrap(), body);
        assert_eq!(fs::metadata(&installed).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::read_link(bin.path().join("cactup")).unwrap(), Path::new("cactup-bbbbbbb"));
        assert_eq!(
            names(bin.path()),
            ["cactup", "cactup-aaaaaaa", "cactup-aaaaaaa.retired", "cactup-bbbbbbb"],
            "no temp files or lock left behind"
        );

        // The race loser: the build is already in place; nothing is fetched
        // (the server is gone, so a request would fail).
        let again = apply_with(&base, &release, &entry, bin.path(), &ME, &mut test_line()).unwrap();
        assert_eq!(again, Applied::AlreadyInstalled);
    }

    #[test]
    fn apply_retires_the_previous_link_target() {
        let bin = tempfile::tempdir().unwrap();
        fs::write(bin.path().join("cactup-0000000"), b"older").unwrap();
        std::os::unix::fs::symlink("cactup-0000000", bin.path().join("cactup")).unwrap();
        // A stamp from an earlier retirement of the build being installed
        // (a rollback, then forward again) must not survive it.
        fs::write(bin.path().join("cactup-bbbbbbb.retired"), b"").unwrap();

        let body = payload("bbbbbbb");
        let entry = entry_for("t/cactup-bbbbbbb", &body);
        let release = latest("bbbbbbb", "2026-09-24T00:00:00+00:00", Some(entry.clone()));
        let (base, server) = test_server::serve(vec![("*", body)], 1);
        let applied = apply_with(&base, &release, &entry, bin.path(), &ME, &mut test_line()).unwrap();
        server.join().unwrap();
        assert!(matches!(applied, Applied::Installed(_)));
        assert_eq!(
            names(bin.path()),
            ["cactup", "cactup-0000000", "cactup-0000000.retired", "cactup-bbbbbbb"]
        );
    }

    #[test]
    fn apply_leaves_bin_alone_until_the_release_has_propagated() {
        let bin = tempfile::tempdir().unwrap();
        fs::write(bin.path().join("cactup"), b"old").unwrap();
        let body = payload("bbbbbbb");
        let entry = entry_for("t/cactup-bbbbbbb", &body);
        let release = latest("bbbbbbb", "2026-09-24T00:00:00+00:00", Some(entry.clone()));

        // The CDN still serves the previous binary under this name: the
        // checksum does not match.
        let (base, server) = test_server::serve(vec![("*", payload("0000000"))], 1);
        let applied = apply_with(&base, &release, &entry, bin.path(), &ME, &mut test_line()).unwrap();
        server.join().unwrap();
        assert_eq!(applied, Applied::NotYet);
        assert_eq!(names(bin.path()), ["cactup"]);
        assert_eq!(fs::read(bin.path().join("cactup")).unwrap(), b"old");

        // Not there at all yet.
        let (base, server) = test_server::serve(vec![], 1);
        let applied = apply_with(&base, &release, &entry, bin.path(), &ME, &mut test_line()).unwrap();
        server.join().unwrap();
        assert_eq!(applied, Applied::NotYet);
        assert_eq!(names(bin.path()), ["cactup"]);
    }

    #[test]
    fn apply_refuses_a_binary_that_does_not_name_its_build() {
        let bin = tempfile::tempdir().unwrap();
        fs::write(bin.path().join("cactup"), b"old").unwrap();
        // Published under bbbbbbb, but it says it is something else.
        let body = payload("0000000");
        let entry = entry_for("t/cactup-bbbbbbb", &body);
        let release = latest("bbbbbbb", "2026-09-24T00:00:00+00:00", Some(entry.clone()));
        let (base, server) = test_server::serve(vec![("*", body)], 1);
        let err = apply_with(&base, &release, &entry, bin.path(), &ME, &mut test_line()).unwrap_err();
        server.join().unwrap();
        assert!(format!("{err:#}").contains("does not name build bbbbbbb"), "{err:#}");
        assert_eq!(names(bin.path()), ["cactup"]);
    }

    #[test]
    fn apply_in_a_read_only_bin_dir_is_not_updatable() {
        let bin = tempfile::tempdir().unwrap();
        fs::write(bin.path().join("cactup"), b"old").unwrap();
        fs::set_permissions(bin.path(), fs::Permissions::from_mode(0o555)).unwrap();
        // root ignores directory permissions; the refusal is untestable then.
        if fs::write(bin.path().join("probe"), b"").is_err() {
            let entry = entry_for("t/cactup-bbbbbbb", b"x");
            let release = latest("bbbbbbb", "2026-09-24T00:00:00+00:00", Some(entry.clone()));
            let applied =
                apply_with("http://127.0.0.1:9", &release, &entry, bin.path(), &ME, &mut test_line())
                    .unwrap();
            let Applied::NotUpdatable(reason) = applied else { panic!("{applied:?}") };
            assert!(reason.contains("not writable"), "{reason}");
        }
        fs::set_permissions(bin.path(), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn apply_steps_aside_for_another_update() {
        let bin = tempfile::tempdir().unwrap();
        let _held = LinkLock::acquire(&bin.path().join(UPDATE_LOCK)).unwrap();
        let entry = entry_for("t/cactup-bbbbbbb", b"x");
        let release = latest("bbbbbbb", "2026-09-24T00:00:00+00:00", Some(entry.clone()));
        let applied =
            apply_with("http://127.0.0.1:9", &release, &entry, bin.path(), &ME, &mut test_line())
                .unwrap();
        assert_eq!(applied, Applied::Busy);
    }

    /// Backdate a file's mtime, as a test fixture only.
    fn age(path: &Path, by: Duration) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - by)
            .unwrap();
    }

    #[test]
    fn prune_removes_only_long_retired_builds() {
        let bin = tempfile::tempdir().unwrap();
        let b = bin.path();
        for build in ["1111111", "2222222", "3333333", "4444444", "5555555", "6666666"] {
            fs::write(b.join(binary_name(build)), build).unwrap();
        }
        std::os::unix::fs::symlink("cactup-6666666", b.join("cactup")).unwrap();
        let old = RETIRED_KEEP + Duration::from_secs(3600);
        // 1: long retired, goes. 2: retired recently, stays. 3: never
        // retired, stays. 4: long retired but running, stays. 5: long
        // retired, binary already gone: the stamp goes. 6: long retired but
        // current (the link target), stays.
        for build in ["1111111", "2222222", "4444444", "5555555", "6666666"] {
            fs::write(b.join(format!("cactup-{build}.retired")), b"").unwrap();
        }
        for build in ["1111111", "4444444", "5555555", "6666666"] {
            age(&b.join(format!("cactup-{build}.retired")), old);
        }
        fs::remove_file(b.join("cactup-5555555")).unwrap();

        let removed = prune_binaries(b, &b.join("cactup-4444444")).unwrap();
        assert_eq!(removed, [b.join("cactup-1111111")]);
        assert_eq!(
            names(b),
            [
                "cactup",
                "cactup-2222222",
                "cactup-2222222.retired",
                "cactup-3333333",
                "cactup-4444444",
                "cactup-4444444.retired",
                "cactup-6666666",
                "cactup-6666666.retired",
            ]
        );
        // Nothing to do in a bin directory that does not exist.
        assert!(prune_binaries(&b.join("missing"), &b.join("cactup")).unwrap().is_empty());
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp.path(), b"abc").unwrap();
        assert_eq!(
            sha256_hex(temp.path()).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
