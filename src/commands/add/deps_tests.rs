//! `cactup add` and `cactup remove` resolving dependencies, end to end: real
//! upstream repos on disk, an installation fetched from them, and a fake
//! Cactarium (`test_server`) serving versions, `/resolve`, `/lookup` and
//! `/providers` in the crate's shapes.
// Plan §8 (integration tests), the P3 row of §7's reviewer checks.

use super::*;
use crate::installation::{AddedFor, InstallationMeta};
use cactup_ecosystem_common::ccl::ThornFacts;
use std::sync::{Arc, Mutex};

/// Every request the fake Cactarium got: its path and body.
type Log = Arc<Mutex<Vec<(String, String)>>>;

/// A thorn of a version: `Arrangement`, `Thorn`, and its facts.
fn thorn(arrangement: &str, name: &str, facts: &ThornFacts) -> String {
    let facts = serde_json::to_string(facts).unwrap();
    format!(r#"{{"arrangement":"{arrangement}","thorn":"{name}","repo-path":"$2","facts":{facts}}}"#)
}

/// One `PackageVersion`, its thorns in arrangement `arrangement`.
fn version(v: &str, commit: &str, url: &str, arrangement: &str, thorns: &[&str], claims: &str) -> String {
    let thorns: Vec<String> = thorns.iter().map(|t| thorn(arrangement, t, &ThornFacts::default())).collect();
    version_with(v, commit, url, &thorns, claims, false)
}

fn version_with(v: &str, commit: &str, url: &str, thorns: &[String], claims: &str, flesh: bool) -> String {
    format!(
        r#"{{"version":"{v}","track":null,"tag":null,"commit":"{commit}","tree":null,"yanked":false,
        "yank-reason":null,"state":"ok","curated":false,"published":0,"url":"{url}","flesh":{flesh},
        "thorns":[{}],"claims":{claims}}}"#,
        thorns.join(",")
    )
}

/// A dependency claim of `target` at `range`, needing `needs`.
fn depends(target: &str, range: &str, needs: &[&str]) -> String {
    let needs: Vec<String> = needs.iter().map(|n| format!("\"{n}\"")).collect();
    format!(r#"{{"kind":"dependency","target":"{target}","range":"{range}","needs":[{}],"note":null}}"#, needs.join(","))
}

/// The fake Cactarium: `packages` (id → its versions, newest first), the
/// pins `/lookup` knows (URL → package and version), and what `/providers`
/// answers with. `lookup`/`providers` false: an older Cactarium, which has
/// no such endpoint (its API fallback answers 404, with its generation).
#[derive(Clone)]
struct Fake {
    packages: Vec<(String, Vec<String>)>,
    pins: Vec<(String, String, Option<String>)>,
    providers: String,
    lookup: bool,
    providers_endpoint: bool,
    /// Reads at a commit no version froze so far: the real server reads the
    /// package now (a fetch of its repository), at most 12 in a window that
    /// opens at the first counted read, then answers 429 (refused reads do
    /// not count).
    unfrozen_reads: usize,
    /// Reads in the current window, and when it opened.
    in_window: usize,
    window_start: Option<std::time::Instant>,
    /// The window's length (60 s on the real server).
    window: std::time::Duration,
    /// Whether a 429 says `Retry-After`.
    retry_after: bool,
    /// Every read at an unfrozen commit answers 429.
    always_limited: bool,
    /// Commits whose reads answer 403 (not served to this caller).
    refuse_at: Vec<String>,
    /// Commits whose reads answer 400 (the package cannot be read there).
    bad_at: Vec<String>,
    /// Commits whose reads answer 502 (the git host is unreachable).
    down_at: Vec<String>,
    /// Repos (by URL) the lookup says are a registered flesh's.
    flesh: Vec<String>,
    /// Commits whose reads a proxy in front gives up on: its own 504, with
    /// no API generation.
    proxy_timeout_at: Vec<String>,
    /// `POST /packages/read` exists (a Cactarium with batch reads).
    batch: bool,
    /// How many packages not in the mirror yet one batch request finishes
    /// fetching within its time; the rest it starts fetching and answers
    /// `not-read`, ready on the next ask. What was fetched stays (warm).
    fetch_budget: usize,
    warm: std::collections::BTreeSet<(String, String)>,
    /// What the server goes on reading after answering, with the asks still
    /// to come before it is ready.
    fetching: std::collections::BTreeMap<(String, String), usize>,
    /// How many later asks a read going on after an answer takes to be ready.
    fetch_asks: usize,
    /// Answers it cannot use: one item short, a body that is no JSON; an
    /// item whose version is at another commit; the package's name echoed in
    /// capitals; an item echoing another commit.
    short_answer: bool,
    garbage_answer: bool,
    wrong_commit_at: Vec<String>,
    shout: bool,
    echo_other_at: Vec<String>,
    /// Items answered untrusted; asks answered with a status this cactup
    /// does not know, before the item is read.
    untrusted_at: Vec<String>,
    novel_status: usize,
    /// How long each batch request takes.
    batch_delay: std::time::Duration,
    /// The next requests that start no fetch (the server's network slots
    /// are all taken by other callers): what is not warm is not read.
    stalled: usize,
    /// Commits whose trees are too large to read (a 400, as the server says).
    too_large_at: Vec<String>,
    /// Batch requests answered, and packages they fetched.
    batch_requests: usize,
    batch_fetches: usize,
    /// What a read at a commit no version froze answers (the package read
    /// now), by package and commit.
    snapshots: Vec<(String, String, String)>,
    /// Stacks: their id and their one release, 1.0.0, as frozen.
    stacks: Vec<(String, String)>,
    /// How long a lookup takes to answer.
    lookup_delay: std::time::Duration,
}

/// A fake Cactarium's state, changeable while it serves.
type Shared = Arc<FakeLock>;

/// The lock on a fake Cactarium's state. A test that waits for it longer
/// than a minute fails, saying why, rather than hang: the state is only ever
/// held for a moment (one answer, one assertion), so a wait that long is a
/// deadlock, such as a statement that locks it twice (a guard lives to the
/// end of its statement, and `std`'s lock is not reentrant).
struct FakeLock(Mutex<Fake>, std::time::Duration);

impl FakeLock {
    /// Shaped as `Mutex::lock` is, so `lock().unwrap()` reads the same.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Fake>, std::convert::Infallible> {
        let deadline = std::time::Instant::now() + self.1;
        loop {
            match self.0.try_lock() {
                Ok(guard) => return Ok(guard),
                // A thread that panicked holding it: that panic is the story.
                Err(std::sync::TryLockError::Poisoned(_)) => panic!("the fake Cactarium's state is poisoned by an earlier panic"),
                Err(std::sync::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    panic!("the fake Cactarium's state stayed locked for {:?}: is it locked twice in one statement?", self.1)
                }
            }
        }
    }
}

impl Fake {
    fn new() -> Fake {
        Fake {
            packages: Vec::new(),
            pins: Vec::new(),
            providers: r#"{"providers":[]}"#.to_owned(),
            lookup: true,
            providers_endpoint: true,
            unfrozen_reads: 0,
            in_window: 0,
            window_start: None,
            window: std::time::Duration::from_secs(60),
            retry_after: false,
            always_limited: false,
            refuse_at: Vec::new(),
            bad_at: Vec::new(),
            down_at: Vec::new(),
            flesh: Vec::new(),
            proxy_timeout_at: Vec::new(),
            batch: true,
            // Most bases read warm: everything within one request.
            fetch_budget: 1000,
            warm: std::collections::BTreeSet::new(),
            fetching: std::collections::BTreeMap::new(),
            fetch_asks: 1,
            short_answer: false,
            garbage_answer: false,
            wrong_commit_at: Vec::new(),
            shout: false,
            echo_other_at: Vec::new(),
            untrusted_at: Vec::new(),
            novel_status: 0,
            batch_delay: std::time::Duration::ZERO,
            stalled: 0,
            too_large_at: Vec::new(),
            batch_requests: 0,
            batch_fetches: 0,
            snapshots: Vec::new(),
            stacks: Vec::new(),
            lookup_delay: std::time::Duration::ZERO,
        }
    }

    fn package(mut self, id: &str, versions: Vec<String>) -> Fake {
        self.packages.push((id.to_owned(), versions));
        self
    }

    fn pin(mut self, url: &str, package: &str, version: Option<&str>) -> Fake {
        self.pins.push((url.to_owned(), package.to_owned(), version.map(str::to_owned)));
        self
    }

    fn serve(self) -> (String, Log) {
        let (base, log, _) = self.serve_shared();
        (base, log)
    }

    fn serve_shared(self) -> (String, Log, Shared) {
        let log: Log = Arc::default();
        let seen = log.clone();
        let state: Shared = Arc::new(FakeLock(Mutex::new(self), std::time::Duration::from_secs(60)));
        let fake = state.clone();
        let (base, _) = crate::fetch::download::test_server::serve_with_body(1000, move |path, body| {
            seen.lock().unwrap().push((path.to_owned(), String::from_utf8_lossy(body).into_owned()));
            fake.lock().unwrap().answer(path, body)
        });
        (base, log, state)
    }

    fn answer(&mut self, path: &str, body: &[u8]) -> (&'static str, String, Vec<u8>) {
        let ok = |body: String| ("200 OK", "api-generation: 1\r\nx-cactarium-untrusted: false\r\n".to_owned(), body.into_bytes());
        let missing = || {
            let body = r#"{"error":{"code":"not-found","message":"No such package"}}"#;
            ("404 Not Found", "api-generation: 1\r\n".to_owned(), body.as_bytes().to_vec())
        };
        // The real server's API fallback: 404, with its API generation.
        let no_route = || {
            let body = r#"{"error":{"code":"not-found","message":"Not found"}}"#;
            ("404 Not Found", "api-generation: 1\r\n".to_owned(), body.as_bytes().to_vec())
        };
        let path = path.strip_prefix("/api/v1").unwrap_or(path);
        if let Some(req) = path.strip_prefix("/resolve?req=") {
            let req = req.split('&').next().unwrap().replace("%40", "@").replace("%23", "#");
            let id = req.split(['@', '#']).next().unwrap();
            if self.stacks.iter().any(|(s, _)| s == id) {
                return ok(format!(
                    r#"{{"slug":"{id}","kind":"stack","url":"u","contact":null,"version":"1.0.0","track":null,
                    "tag":null,"commit":null,"tree":null,"yanked":false,"yank-reason":null,"state":"ok",
                    "curated":false,"published":0}}"#
                ));
            }
            return match self.packages.iter().find(|(p, _)| p == id) {
                Some(_) => ok(format!(
                    r#"{{"slug":"{id}","kind":"package","url":"u","contact":null,"version":"1.0.0","track":null,
                    "tag":null,"commit":null,"tree":null,"yanked":false,"yank-reason":null,"state":"ok",
                    "curated":false,"published":0}}"#
                )),
                None => missing(),
            };
        }
        if let Some(rest) = path.strip_prefix("/stacks/") {
            let id = rest.strip_suffix("/releases/1.0.0").unwrap_or(rest);
            return match self.stacks.iter().find(|(s, _)| s == id) {
                Some((_, frozen)) => ok(frozen.clone()),
                None => missing(),
            };
        }
        if path == "/lookup" {
            std::thread::sleep(self.lookup_delay);
            if !self.lookup {
                return no_route();
            }
            let asked: cactup_ecosystem_common::api::LookupRequest = serde_json::from_slice(body).unwrap();
            let pins: Vec<String> = (asked.pins.iter())
                .map(|p| {
                    let known = self.pins.iter().find(|(url, _, _)| *url == p.url);
                    let package = known.map_or("null".to_owned(), |(_, pkg, _)| format!("\"{pkg}\""));
                    // The version that froze the commit, as the server says:
                    // null when none did.
                    let version = known
                        .and_then(|(_, pkg, _)| self.frozen(pkg, &p.commit))
                        .map_or("null".to_owned(), |v| format!("\"{v}\""));
                    // The real server often cannot say whether its mirror has
                    // the commit: null.
                    let flesh = self.flesh.contains(&p.url);
                    format!(
                        r#"{{"url":"{}","commit":"{}","package":{package},"version":{version},"flesh":{flesh},
                        "fork-of":null,"commit-known":null}}"#,
                        p.url, p.commit
                    )
                })
                .collect();
            return ok(format!(r#"{{"pins":[{}]}}"#, pins.join(",")));
        }
        if path.starts_with("/providers?") {
            return if self.providers_endpoint { ok(self.providers.clone()) } else { no_route() };
        }
        if path == "/packages/read" {
            if !self.batch {
                return no_route();
            }
            return self.read_batch(body);
        }
        if let Some(rest) = path.strip_prefix("/packages/") {
            let (id, at) = match rest.split_once("/versions") {
                Some((id, at)) => (id, at.trim_start_matches('/')),
                None => return missing(),
            };
            let Some((_, versions)) = self.packages.iter().find(|(p, _)| p == id) else { return missing() };
            if at.is_empty() {
                return ok(format!(r#"{{"slug":"{id}","versions":[{}]}}"#, versions.join(",")));
            }
            // The statuses of a read at a commit, as the server answers them.
            let error = |status: &'static str, code: &str, message: String| {
                let body = serde_json::json!({"error": {"code": code, "message": message}}).to_string();
                (status, "api-generation: 1\r\n".to_owned(), body.into_bytes())
            };
            if self.refuse_at.iter().any(|c| c == at) {
                return error("403 Forbidden", "forbidden", "not served: the package isn't trusted".into());
            }
            // An abbreviated commit too short to look up; one that matches
            // nothing is a 404 below, as any commit the repo lacks.
            if at.len() < 7 && at.chars().all(|c| c.is_ascii_hexdigit()) {
                return error("400 Bad Request", "bad-request", "abbreviated commits need at least 7 hex digits".into());
            }
            if self.bad_at.iter().any(|c| c == at) {
                let message = format!("{id} at commit {}: invalid manifest: no [package] table", &at[..12]);
                return error("400 Bad Request", "bad-request", message);
            }
            if self.too_large_at.iter().any(|c| c == at) {
                let message = format!("{id} at commit {}: its tree is too large to read", &at[..12]);
                return error("400 Bad Request", "bad-request", message);
            }
            if self.proxy_timeout_at.iter().any(|c| c == at) {
                return ("504 Gateway Timeout", String::new(), b"<html><body>Gateway Timeout</body></html>".to_vec());
            }
            if self.down_at.iter().any(|c| c == at) {
                return error("502 Bad Gateway", "upstream", "the git host did not answer".into());
            }
            // Several versions at one commit (tracks): this fake answers the
            // last listed, as a server might any of them.
            if let Some(v) = versions.iter().rev().find(|v| v.contains(&format!("\"commit\":\"{at}"))) {
                return ok(v.clone());
            }
            let now = std::time::Instant::now();
            if self.window_start.is_some_and(|s| now.duration_since(s) >= self.window) {
                (self.window_start, self.in_window) = (None, 0);
            }
            if self.always_limited || self.in_window >= 12 {
                let body = r#"{"error":{"code":"rate-limited","message":"Too many requests; try again in a minute"}}"#;
                let left = self.window_start.map_or(self.window, |s| self.window.saturating_sub(now.duration_since(s)));
                let header = if self.retry_after { format!("retry-after: {}\r\n", left.as_secs().max(1)) } else { String::new() };
                return ("429 Too Many Requests", format!("api-generation: 1\r\n{header}"), body.as_bytes().to_vec());
            }
            self.window_start.get_or_insert(now);
            self.in_window += 1;
            self.unfrozen_reads += 1;
            return match self.snapshots.iter().find(|(p, c, _)| p == id && c == at) {
                Some((_, _, json)) => ok(json.clone()),
                None => error("404 Not Found", "not-found", format!("no commit {at} in {id}'s repository (is it pushed?)")),
            };
        }
        no_route()
    }
}

impl Fake {
    /// `POST /packages/read`, as the server answers it: the whole request
    /// counts once against the window of reads made now (a 429 refuses all
    /// of it); each item as the single read would answer, but a package not
    /// fetched yet is fetched only within the request's budget, and answers
    /// `not-read` past it (or while the fetch slots are held elsewhere).
    fn read_batch(&mut self, body: &[u8]) -> (&'static str, String, Vec<u8>) {
        let asked: cactup_ecosystem_common::api::ReadRequest = serde_json::from_slice(body).unwrap();
        asked.validate().expect("a valid read request");
        // A proxy giving up on a request that reads one of these.
        if asked.items.iter().any(|q| self.proxy_timeout_at.contains(&q.commit)) {
            return ("504 Gateway Timeout", String::new(), b"<html><body>Gateway Timeout</body></html>".to_vec());
        }
        let now = std::time::Instant::now();
        if self.window_start.is_some_and(|s| now.duration_since(s) >= self.window) {
            (self.window_start, self.in_window) = (None, 0);
        }
        if self.always_limited || self.in_window >= 12 {
            let body = r#"{"error":{"code":"rate-limited","message":"Too many requests; try again in a minute"}}"#;
            let left = self.window_start.map_or(self.window, |s| self.window.saturating_sub(now.duration_since(s)));
            let header = if self.retry_after { format!("retry-after: {}\r\n", left.as_secs().max(1)) } else { String::new() };
            return ("429 Too Many Requests", format!("api-generation: 1\r\n{header}"), body.as_bytes().to_vec());
        }
        self.window_start.get_or_insert(now);
        self.in_window += 1;
        self.batch_requests += 1;
        std::thread::sleep(self.batch_delay);
        let stalled = self.stalled > 0;
        self.stalled = self.stalled.saturating_sub(1);
        // What earlier requests left reading is one ask nearer being ready.
        for left in self.fetching.values_mut() {
            *left = left.saturating_sub(1);
        }
        let ready: Vec<(String, String)> = self.fetching.iter().filter(|(_, l)| **l == 0).map(|(k, _)| k.clone()).collect();
        for key in ready {
            self.fetching.remove(&key);
            self.warm.insert(key);
        }
        let mut fetched = 0;
        let mut items = Vec::new();
        for q in &asked.items {
            let (id, at) = (q.package.as_str(), q.commit.as_str());
            let fetching = self.fetching.contains_key(&(id.to_owned(), at.to_owned()));
            let (echo_id, echo_at) = (
                if self.shout { id.to_ascii_uppercase() } else { id.to_owned() },
                if self.echo_other_at.iter().any(|c| c == at) { "f".repeat(40) } else { at.to_owned() },
            );
            let wrong_commit = self.wrong_commit_at.iter().any(|c| c == at);
            let untrusted = self.untrusted_at.iter().any(|c| c == at);
            let item = |status: &str, version: Option<serde_json::Value>, error: Option<String>| {
                let in_progress = status == "not-read" && (fetching || error.as_deref() == Some("reading on"));
                let mut version = version;
                if let (Some(v), true) = (version.as_mut(), wrong_commit) {
                    v["commit"] = serde_json::json!("e".repeat(40));
                }
                serde_json::json!({"package": echo_id, "commit": echo_at, "status": status, "version": version,
                    "error": error, "untrusted": untrusted, "unreviewed": false, "in-progress": in_progress})
            };
            let frozen = (self.packages.iter().find(|(p, _)| p == id))
                .and_then(|(_, vs)| vs.iter().rev().find(|v| v.contains(&format!("\"commit\":\"{at}"))).cloned());
            items.push(if self.novel_status > 0 {
                item("queued", None, Some("a status from a newer server".into()))
            } else if self.refuse_at.iter().any(|c| c == at) {
                item("forbidden", None, Some("not served: the package isn't trusted".into()))
            } else if self.bad_at.iter().any(|c| c == at) {
                item("bad-request", None, Some(format!("{id} at commit {}: invalid manifest: no [package] table", &at[..12])))
            } else if self.too_large_at.iter().any(|c| c == at) {
                item("bad-request", None, Some(format!("{id} at commit {}: its tree is too large to read", &at[..12])))
            } else if self.down_at.iter().any(|c| c == at) {
                item("upstream", None, Some("the git host did not answer".into()))
            } else if let Some(v) = frozen {
                item("ok", Some(serde_json::from_str(&v).unwrap()), None)
            } else {
                match self.snapshots.iter().find(|(p, c, _)| p == id && c == at).map(|(_, _, j)| j.clone()) {
                    None => item("not-found", None, Some(format!("no commit {at} in {id}'s repository (is it pushed?)"))),
                    Some(json) => {
                        let key = (id.to_owned(), at.to_owned());
                        if self.warm.contains(&key) {
                            item("ok", Some(serde_json::from_str(&json).unwrap()), None)
                        } else if stalled {
                            item("not-read", None, Some("not read this time".into()))
                        } else if fetched < self.fetch_budget {
                            fetched += 1;
                            self.batch_fetches += 1;
                            self.warm.insert(key);
                            item("ok", Some(serde_json::from_str(&json).unwrap()), None)
                        } else {
                            // Read on after the answer: ready in `fetch_asks` asks.
                            if !self.fetching.contains_key(&key) {
                                self.fetching.insert(key, self.fetch_asks);
                                self.batch_fetches += 1;
                            }
                            item("not-read", None, Some("reading on".into()))
                        }
                    }
                }
            });
        }
        self.novel_status = self.novel_status.saturating_sub(1);
        if self.short_answer {
            items.pop();
        }
        let body = if self.garbage_answer { "{\"items\": [".to_owned() } else { serde_json::json!({ "items": items }).to_string() };
        ("200 OK", "api-generation: 1\r\n".to_owned(), body.into_bytes())
    }

    /// The version of `package` that froze `commit`, if one did.
    /// On the default track first, as the server's lookup answers.
    fn frozen(&self, package: &str, commit: &str) -> Option<String> {
        let (_, versions) = self.packages.iter().find(|(p, _)| p == package)?;
        let label = |v: &String| {
            let start = v.find("\"version\":\"")? + "\"version\":\"".len();
            Some(v[start..].split('"').next()?.to_owned())
        };
        let at: Vec<String> =
            versions.iter().filter(|v| v.contains(&format!("\"commit\":\"{commit}\""))).filter_map(label).collect();
        at.iter().find(|v| !v.contains(':')).or(at.first()).cloned()
    }
}

/// The requests the fake got, by kind: `/resolve`, `/lookup`, whole
/// version lists, one version at a commit, `/providers`.
fn counts(log: &Log) -> [usize; 5] {
    let mut n = [0; 5];
    for (path, _) in log.lock().unwrap().iter() {
        let path = path.strip_prefix("/api/v1").unwrap_or(path);
        let i = match path {
            p if p.starts_with("/resolve") => 0,
            "/lookup" => 1,
            p if p.ends_with("/versions") => 2,
            p if p.contains("/versions/") => 3,
            _ => 4,
        };
        n[i] += 1;
    }
    n
}

/// An upstream repo `name` with `files`, as a local URL and its commit.
fn upstream(tmp: &std::path::Path, name: &str, files: &[(&str, &str)]) -> (String, String) {
    let dir = tmp.join("up").join(name);
    if !dir.exists() {
        crate::fetch::git::testrepo::init(&dir);
    }
    (dir.display().to_string(), crate::fetch::tests::commit_files(&dir, files).to_string())
}

/// A base thornlist section: `url` at `commit`, checking out `checkouts`.
fn section(url: &str, commit: &str, checkouts: &[&str]) -> String {
    format!(
        "\n!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = {url}\n!REPO_PATH = $2\n!REPO_COMMIT = {commit}\n\
         !CHECKOUT =\n{}\n",
        checkouts.join("\n")
    )
}

/// An installation (alias `t`, active) of `base`, fetched, with
/// Cactarium at `cactarium`.
struct Setup {
    _tmp: tempfile::TempDir,
    ctx: Ctx,
    inst: Installation,
}

impl Setup {
    fn new(tmp: tempfile::TempDir, base: &str, cactarium: &str) -> Setup {
        let root = tmp.path().join("inst");
        let db = crate::database::Db::in_dir(&tmp.path().join("db"));
        let path = root.display().to_string();
        let cactarium = cactarium.to_owned();
        db.update(move |d| {
            d.installations.insert(
                "t".into(),
                crate::database::CactusInstallation {
                    alias: "t".into(),
                    release: None,
                    path,
                    thornlist: None,
                    simfile: None,
                    stack: None,
                    current_release: None,
                    current_thornlist: None,
                    stack_contact: None,
                    unfetched_repos: Default::default(),
                    copied_from: None,
                },
            );
            d.active_installation = Some("t".into());
            d.set_knob(crate::cactarium::URL_KNOB, cactarium);
            Ok(())
        })
        .unwrap();
        let ctx = Ctx {
            globals: crate::args::GlobalOpts {
                verbose: false,
                trace: false,
                manifest_url: String::new(),
                mdb_path: None,
                machine: None,
                installation: None,
                hostname: None,
                knob: Vec::new(),
                risky_allow_untrusted_artifacts: false,
            },
            db,
        };
        let inst = Installation::resolve(&ctx).unwrap();
        let pristine = format!("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{base}");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(inst.source_thornlist(), &pristine).unwrap();
        let list = thornlist::parse(&pristine).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("plan");
        let plan = crate::fetch::plan(&list, &inst.root, &mut progress).unwrap();
        let report = crate::fetch::execute(&plan, &inst.root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        crate::fetch::FetchState::record(&inst.root, &report.records()).unwrap();
        let live = inst.live_thornlist().unwrap();
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, &pristine).unwrap();
        Setup { _tmp: tmp, ctx, inst }
    }

    fn meta(&self) -> InstallationMeta {
        self.inst.meta().unwrap()
    }

    fn added(&self) -> Vec<(String, Vec<String>)> {
        (self.meta().added.iter())
            .map(|a| (a.package.clone(), a.thorns.iter().map(|t| t.checkout.clone()).collect()))
            .collect()
    }

    fn add(&self, specs: &[&str], flags: AddFlags) -> Res<()> {
        let specs: Vec<String> = specs.iter().map(|s| (*s).to_owned()).collect();
        dispatch(&self.ctx, &specs, &flags)
    }

    fn remove(&self, packages: &[&str], flags: RemoveFlags) -> Res<()> {
        let packages: Vec<String> = packages.iter().map(|s| (*s).to_owned()).collect();
        remove(&self.ctx, &packages, &flags)
    }

    /// Rewrites the record by hand (as an older binary, or a stale run,
    /// would leave it), keeping whatever `set_meta` would drop.
    fn write_meta(&self, meta: &InstallationMeta) {
        let path = self.inst.cactup_dir().join("installation.toml");
        crate::format::write(&path, meta, &crate::installation::INSTALLATION).unwrap();
    }
}

fn yes() -> AddFlags {
    AddFlags { yes: true, ..AddFlags::default() }
}

fn remove_yes() -> RemoveFlags {
    RemoveFlags { yes: true, ..RemoveFlags::default() }
}

/// max/z4c 1.0.0 (thorn Z4c) depends on et/carpetx ^1.0 for its CarpetX and
/// Loop (et/carpetx has Arith too); the installation's base is empty.
struct Z4c {
    setup: Setup,
    z: String,
    c: String,
    log: Log,
    fake: Shared,
}

fn z4c() -> Z4c {
    let tmp = tempfile::tempdir().unwrap();
    let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\n")]);
    let (curl, c) = upstream(
        tmp.path(),
        "carpetx",
        &[
            ("CarpetX/interface.ccl", "IMPLEMENTS: carpetx\n"),
            ("Loop/interface.ccl", "IMPLEMENTS: loop\n"),
            ("Arith/interface.ccl", "IMPLEMENTS: arith\n"),
        ],
    );
    let claims = format!("[{}]", depends("et/carpetx", "^1.0", &["CarpetX", "Loop"]));
    let (base, log, fake) = Fake::new()
        .package("max/z4c", vec![version("1.0.0", &z, &zurl, "Z4c", &["Z4c"], &claims)])
        .package("et/carpetx", vec![version("1.0.0", &c, &curl, "CarpetX", &["CarpetX", "Loop", "Arith"], "[]")])
        .serve_shared();
    Z4c { setup: Setup::new(tmp, "", &base), z, c, log, fake }
}

/// `cactup add max/z4c`: the plan brings in et/carpetx's CarpetX and Loop,
/// so it is shown and confirmed; without a terminal it is refused unless
/// `--yes`, `--dry-run` changes nothing, and with `--yes` both are added,
/// carpetx's thorns into carpetx's own clone, each with why it is there.
#[test]
fn add_brings_in_the_needs_of_a_dependency() {
    let it = z4c();
    let s = &it.setup;
    let err = format!("{:#}", s.add(&["max/z4c"], AddFlags::default()).unwrap_err());
    assert!(err.contains("Pass --yes, or --dry-run to see the plan"), "{err}");
    assert!(s.meta().added.is_empty());
    s.add(&["max/z4c"], AddFlags { dry_run: true, ..AddFlags::default() }).unwrap();
    assert!(s.meta().added.is_empty());

    s.add(&["max/z4c"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(
        s.added(),
        [
            ("max/z4c".to_owned(), vec!["Z4c/Z4c".to_owned()]),
            ("et/carpetx".to_owned(), vec!["CarpetX/CarpetX".to_owned(), "CarpetX/Loop".to_owned()]),
        ]
    );
    let meta = s.meta();
    let carpetx = meta.added.iter().find(|a| a.package == "et/carpetx").unwrap();
    assert_eq!((carpetx.repo.as_str(), carpetx.commit.as_str()), ("carpetx", it.c.as_str()));
    let arrangements = s.inst.root.join("Cactus/arrangements");
    assert_eq!(std::fs::read_to_string(arrangements.join("CarpetX/Loop/interface.ccl")).unwrap(), "IMPLEMENTS: loop\n");
    assert!(!arrangements.join("CarpetX/Arith").exists(), "only what the dependency needs");
    assert_eq!(
        meta.added_for["max/z4c"],
        AddedFor { requested: true, by: vec![], for_commit: Some(it.z.clone()), choices: vec![] }
    );
    assert_eq!(
        meta.added_for["et/carpetx"],
        AddedFor { requested: false, by: vec!["max/z4c@1.0.0".to_owned()], for_commit: Some(it.c.clone()), choices: vec![] }
    );
    // No pins in the base: nothing to look up.
    assert!(!it.log.lock().unwrap().iter().any(|(p, _)| p.contains("/lookup")));
}

/// `cactup remove`: a package another depends on is refused, naming the
/// dependent; removing the dependent takes along what only it needed, once
/// confirmed.
#[test]
fn remove_refuses_a_dependency_and_takes_orphans_along() {
    let it = z4c();
    let s = &it.setup;
    s.add(&["max/z4c"], yes()).unwrap();

    let err = format!("{:#}", s.remove(&["et/carpetx"], remove_yes()).unwrap_err());
    assert!(err.contains("Other packages depend on et/carpetx") && err.contains("max/z4c@1.0.0"), "{err}");
    assert!(err.contains("--no-deps"), "{err}");
    assert_eq!(s.meta().added.len(), 2);

    let err = format!("{:#}", s.remove(&["max/z4c"], RemoveFlags::default()).unwrap_err());
    assert!(err.contains("Pass --yes, or --dry-run"), "{err}");
    s.remove(&["max/z4c"], RemoveFlags { dry_run: true, ..RemoveFlags::default() }).unwrap();
    assert_eq!(s.meta().added.len(), 2);

    s.remove(&["max/z4c"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let meta = s.meta();
    assert!(meta.added.is_empty() && meta.added_for.is_empty(), "{meta:?}");
}

/// A package whose `added-for` entry is missing (an installation from
/// before entries were kept) or stale (written for another commit: an older
/// binary re-added it) counts as named: no orphan pass takes it.
#[test]
fn a_missing_or_stale_added_for_never_orphans_a_package() {
    for stale in [false, true] {
        let it = z4c();
        let s = &it.setup;
        s.add(&["max/z4c"], yes()).unwrap();
        let mut meta = s.meta();
        if stale {
            meta.added_for.get_mut("et/carpetx").unwrap().for_commit = Some("f".repeat(40));
        } else {
            meta.added_for.remove("et/carpetx");
        }
        s.write_meta(&meta);
        s.remove(&["max/z4c"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(s.added(), [("et/carpetx".to_owned(), vec!["CarpetX/CarpetX".to_owned(), "CarpetX/Loop".to_owned()])]);
    }
}

/// Without Cactarium, `remove` cannot tell what depends on what: it
/// refuses, naming `--no-deps`, which removes exactly what was named.
#[test]
fn remove_offline_refuses_unless_no_deps() {
    let it = z4c();
    let s = &it.setup;
    s.add(&["max/z4c"], yes()).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    s.ctx
        .db
        .update(move |d| {
            d.set_knob(crate::cactarium::URL_KNOB, format!("http://127.0.0.1:{port}"));
            Ok(())
        })
        .unwrap();
    let err = format!("{:#}", s.remove(&["max/z4c"], remove_yes()).unwrap_err());
    assert!(err.contains("isn't reachable") && err.contains("Pass --no-deps"), "{err}");
    assert_eq!(s.meta().added.len(), 2);
    s.remove(&["max/z4c"], RemoveFlags { no_deps: true, ..RemoveFlags::default() }).unwrap();
    assert_eq!(s.added(), [("et/carpetx".to_owned(), vec!["CarpetX/CarpetX".to_owned(), "CarpetX/Loop".to_owned()])]);
}

/// A base of et/admbase (ADMBase, ADMCoupling) at 1.0.0 whose repo also has
/// StaticConformal and ADMMacros, and 2.0.0 a commit later; max/app 1.0.0
/// needs StaticConformal of et/admbase `range`, and max/app2 1.0.0 needs its
/// ADMBase and ADMMacros at ^1.0. max/solo 1.0.0 needs nothing.
struct Admbase {
    setup: Setup,
    a1: String,
    a2: String,
    log: Log,
    fake: Shared,
}

fn admbase(range: &str) -> Admbase {
    admbase_with(range, |f| f)
}

fn admbase_with(range: &str, change: impl FnOnce(Fake) -> Fake) -> Admbase {
    admbase_full(range, true, true, change)
}

/// [`admbase`], with the base at a1 no version froze: 1.0.0 froze another
/// commit, and a read at a1 answers the package read there.
fn admbase_unfrozen(range: &str) -> Admbase {
    admbase_full(range, true, false, |f| f)
}

/// [`admbase_with`], its base section pinned or following its branch.
fn admbase_full(range: &str, pinned: bool, frozen: bool, change: impl FnOnce(Fake) -> Fake) -> Admbase {
    let tmp = tempfile::tempdir().unwrap();
    let files = [
        ("ADMBase/interface.ccl", "IMPLEMENTS: admbase\n"),
        ("ADMCoupling/interface.ccl", "IMPLEMENTS: admcoupling\n"),
        ("StaticConformal/interface.ccl", "IMPLEMENTS: staticconformal\n"),
        ("ADMMacros/interface.ccl", "IMPLEMENTS: admmacros\n"),
    ];
    let (aurl, a1) = upstream(tmp.path(), "admbase", &files);
    let (_, a2) = upstream(tmp.path(), "admbase", &[files[0], files[1], files[2], files[3], ("README", "2\n")]);
    let (purl, p) = upstream(tmp.path(), "app", &[("App/interface.ccl", "IMPLEMENTS: app\n")]);
    let (p2url, p2) = upstream(tmp.path(), "app2", &[("App2/interface.ccl", "IMPLEMENTS: app2\n")]);
    let (surl, so) = upstream(tmp.path(), "solo", &[("Solo/interface.ccl", "IMPLEMENTS: solo\n")]);
    let (p3url, p3) = upstream(tmp.path(), "app3", &[("App3/interface.ccl", "IMPLEMENTS: app3\n")]);
    let claims3 = format!("[{}]", depends("et/admbase", ">=1.0", &["ADMMacros"]));
    let thorns = ["ADMBase", "ADMCoupling", "StaticConformal", "ADMMacros"];
    let claims = format!("[{}]", depends("et/admbase", range, &["StaticConformal"]));
    let claims2 = format!("[{}]", depends("et/admbase", "^1.0", &["ADMBase", "ADMMacros"]));
    let one = if frozen { a1.clone() } else { "1".repeat(40) };
    let mut fake = Fake::new();
    if !frozen {
        let read = version(&a1, &a1, &aurl, "ADMBase", &thorns, "[]");
        fake.snapshots.push(("et/admbase".to_owned(), a1.clone(), read));
    }
    let fake = fake
        .package("max/app", vec![version("1.0.0", &p, &purl, "App", &["App"], &claims)])
        .package("max/app2", vec![version("1.0.0", &p2, &p2url, "App2", &["App2"], &claims2)])
        .package("max/solo", vec![version("1.0.0", &so, &surl, "Solo", &["Solo"], "[]")])
        .package("max/app3", vec![version("1.0.0", &p3, &p3url, "App3", &["App3"], &claims3)])
        .package(
            "et/admbase",
            vec![
                version("2.0.0", &a2, &aurl, "ADMBase", &thorns, "[]"),
                version("1.0.0", &one, &aurl, "ADMBase", &thorns, "[]"),
            ],
        )
        .pin(&aurl, "et/admbase", Some("1.0.0"));
    let (base, log, fake) = change(fake).serve_shared();
    let mut text = section(&aurl, &a1, &["ADMBase/ADMBase", "ADMBase/ADMCoupling"]);
    if !pinned {
        text = text.replace(&format!("!REPO_COMMIT = {a1}\n"), "");
    }
    let setup = Setup::new(tmp, &text, &base);
    Admbase { setup, a1, a2, log, fake }
}

/// The plan for `specs` on the setup's installation, and the records it
/// writes.
fn records(s: &Setup, specs: &[&str], flags: &AddFlags) -> Res<Vec<AddedPackage>> {
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let base = thornlist::parse(&pristine).unwrap();
    let meta = s.meta();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    let specs: Vec<String> = specs.iter().map(|s| (*s).to_owned()).collect();
    let (plan, present, heads) = plan_add(&client, &s.inst.root, &base, &meta, None, &specs, flags)?;
    let records = records_of(&plan, &present, &meta, &base)?;
    place(&base, &meta, records, &plan, &mut Vec::new(), &heads)
}

/// The record of `package` among `records`: version, commit, thorns, replaces.
fn record_of(records: &[AddedPackage], package: &str) -> (String, String, Vec<String>, Vec<String>, String) {
    let r = records.iter().find(|r| r.package == package).unwrap_or_else(|| panic!("no {package} in {records:?}"));
    let thorns = r.thorns.iter().map(|t| t.checkout.clone()).collect();
    (r.version.clone(), r.commit.clone(), thorns, r.replaces.clone(), r.repo.clone())
}

/// A dependency that moves a base package: refused without `--upgrade`,
/// which names the flag; with it, the package moves carrying every thorn
/// the installation had enabled plus the new one, into the base's own
/// clone, in place of the base repo. `--upgrade-all`, naming the package
/// itself under `--upgrade`, and naming the version with `--replace` all
/// write that same record.
#[test]
fn upgrades_carry_the_present_enabled_set_and_the_flags_agree() {
    let it = admbase("^2.0");
    let s = &it.setup;
    let err = format!("{:#}", records(s, &["max/app"], &AddFlags::default()).unwrap_err());
    assert!(err.contains("et/admbase@1.0.0") && err.contains("Pass --upgrade"), "{err}");

    let want = (
        "2.0.0".to_owned(),
        it.a2.clone(),
        vec!["ADMBase/ADMBase".to_owned(), "ADMBase/ADMCoupling".to_owned(), "ADMBase/StaticConformal".to_owned()],
        vec!["admbase".to_owned()],
        "admbase".to_owned(),
    );
    let sorted = |mut r: (String, String, Vec<String>, Vec<String>, String)| {
        r.2.sort();
        r
    };
    let upgrade = AddFlags { upgrade: true, ..AddFlags::default() };
    let got = records(s, &["max/app"], &upgrade).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(sorted(record_of(&got, "et/admbase")), want);
    let all = AddFlags { upgrade_all: true, ..AddFlags::default() };
    let got = records(s, &["max/app"], &all).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(sorted(record_of(&got, "et/admbase")), want);
    let got = records(s, &["et/admbase/StaticConformal"], &upgrade).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(sorted(record_of(&got, "et/admbase")), want);
    let replace = AddFlags { replace: true, ..AddFlags::default() };
    let got = records(s, &["et/admbase/StaticConformal@2.0.0"], &replace).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(sorted(record_of(&got, "et/admbase")), want);

    // And on disk: the base's clone moves to 2.0.0 with those three thorns.
    s.add(&["max/app"], AddFlags { upgrade: true, yes: true, ..AddFlags::default() }).unwrap_or_else(|e| panic!("{e:#}"));
    let arrangements = s.inst.root.join("Cactus/arrangements/ADMBase");
    assert!(arrangements.join("StaticConformal/interface.ccl").is_file());
    assert!(arrangements.join("ADMCoupling/interface.ccl").is_file(), "the enabled thorn stays");
    let state = crate::fetch::FetchState::read(&s.inst.root).unwrap().unwrap();
    assert_eq!(state.repos["admbase"].head, it.a2);
    let _ = &it.a1;
}

/// Every pinned repo of the base goes out in one `/lookup` request (the
/// endpoint is rate-limited), and a `commit-known` of null (the server could
/// not check its mirror) still maps the repo: nothing reads it as unpushed.
#[test]
fn the_base_is_looked_up_in_one_request_and_null_commit_known_maps() {
    let tmp = tempfile::tempdir().unwrap();
    let mut base = String::new();
    let mut urls = Vec::new();
    for name in ["one", "two", "three", "four", "five"] {
        let arrangement = format!("Arr{name}");
        let (url, c) = upstream(tmp.path(), name, &[(&format!("T{name}/interface.ccl"), "IMPLEMENTS: x\n")]);
        base.push_str(&section(&url, &c, &[&format!("{arrangement}/T{name}")]));
        urls.push((url, c));
    }
    let (aurl, a) = (urls[0].0.clone(), urls[0].1.clone());
    let (fake, log) = Fake::new()
        .package("et/one", vec![version("1.0.0", &a, &aurl, "Arrone", &["Tone", "Extra"], "[]")])
        .pin(&aurl, "et/one", Some("1.0.0"))
        .serve();
    let s = Setup::new(tmp, &base, &fake);
    // Base repo one is et/one at 1.0.0, read at its commit; the rest are no
    // registered package's.
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let list = thornlist::parse(&pristine).unwrap();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    let mut reg = super::super::resolution::Registry::new(&client, []);
    let read = super::super::resolution::present(&mut reg, &s.inst.root, &list, &[], None, &Default::default());
    reg.finish();
    let read = read.unwrap_or_else(|e| panic!("{e:#}"));
    let ids: Vec<(String, String)> = (read.present.iter())
        .map(|p| (p.release.id.to_string(), p.release.label()))
        .collect();
    assert!(ids.contains(&("et/one".to_owned(), "et/one@1.0.0".to_owned())), "{ids:?}");
    assert_eq!(ids.iter().filter(|(id, _)| id.starts_with("local:")).count(), 4, "{ids:?}");
    // Naming a base package: it is in the base already, so its new thorn
    // goes into the base's own clone.
    log.lock().unwrap().clear();
    let got = records(&s, &["et/one/Extra"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(record_of(&got, "et/one").2, ["Arrone/Extra"]);
    let lookups: Vec<String> =
        log.lock().unwrap().iter().filter(|(p, _)| p.ends_with("/lookup")).map(|(_, b)| b.clone()).collect();
    assert_eq!(lookups.len(), 1, "{lookups:?}");
    let asked: cactup_ecosystem_common::api::LookupRequest = serde_json::from_str(&lookups[0]).unwrap();
    assert_eq!(asked.pins.len(), 5);
    assert!(urls.iter().all(|(u, c)| asked.pins.iter().any(|p| &p.url == u && &p.commit == c)));
}

/// More thorns of a base package go into an `[[added]]` entry of their own,
/// at the base's commit, brought in (not named); an `added-for` entry an
/// older binary's `remove` left for it (saying it was named, for that very
/// commit, by another dependent) is not inherited: a package with no
/// `[[added]]` entry before starts afresh.
#[test]
fn a_leftover_added_for_entry_is_not_inherited() {
    let it = admbase("^1.0");
    let s = &it.setup;
    let mut meta = s.meta();
    let leftover = AddedFor { requested: true, by: vec!["max/old@1.0.0".into()], for_commit: Some(it.a1.clone()), choices: vec![] };
    meta.added_for.insert("et/admbase".into(), leftover);
    s.write_meta(&meta);
    s.add(&["max/app"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let meta = s.meta();
    let admbase = meta.added.iter().find(|a| a.package == "et/admbase").unwrap();
    assert_eq!((admbase.commit.as_str(), admbase.repo.as_str()), (it.a1.as_str(), "admbase"));
    let entry = &meta.added_for["et/admbase"];
    assert!(!entry.requested, "{entry:?}");
    assert_eq!(entry.by, ["max/app@1.0.0"], "{entry:?}");
    assert!(s.inst.root.join("Cactus/arrangements/ADMBase/StaticConformal/interface.ccl").is_file());
    let _ = (&it.a2, &it.log);
}

/// A flesh package takes over the base flesh's items at the root: a base
/// with no flesh has nothing for it to take the place of, and is refused
/// before anything is shown or changed, naming what needs it when a
/// dependency brings it in.
#[test]
fn a_flesh_package_needs_a_base_flesh_to_replace() {
    let flesh = version_with("1.0.0", &"a".repeat(40), "https://x/cactus.git", &[], "[]", true);
    let claims = format!("[{}]", depends("et/cactus", ">=1.0", &[]));
    let user = version("1.0.0", &"b".repeat(40), "https://x/u.git", "U", &["U"], &claims);
    let (base, _) = Fake::new().package("et/cactus", vec![flesh]).package("max/u", vec![user]).serve();
    let tmp = tempfile::tempdir().unwrap();
    let s = Setup::new(tmp, "", &base);
    let err = format!("{:#}", s.add(&["et/cactus"], yes()).unwrap_err());
    assert!(err.starts_with("et/cactus@1.0.0 is a Cactus flesh.\nThe installation's thornlist has no flesh to replace."), "{err}");
    let err = format!("{:#}", s.add(&["max/u"], yes()).unwrap_err());
    assert!(err.contains("et/cactus@1.0.0 is a Cactus flesh.\nmax/u@1.0.0 needs it.\nThe installation"), "{err}");
    assert!(s.meta().added.is_empty());
}

/// A thorn taken over from a base repo is taken at the base's own
/// `Arrangement/Thorn`: a fork that keeps it in another arrangement is
/// refused, since compose would keep both.
#[test]
fn a_replaced_thorn_must_keep_the_base_s_checkout() {
    use crate::deps::{PkgId, Plan, Replaced, Step};
    let base = thornlist::parse(&format!(
        "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{}",
        section("https://x/carpetx.git", &"c".repeat(40), &["CarpetX/CarpetX", "CarpetX/Loop"])
    ))
    .unwrap();
    let fork = |arrangement: &str| AddedPackage {
        package: "max/carpetx-mp".into(),
        version: "1.0.0".into(),
        tag: None,
        commit: "d".repeat(40),
        url: "https://x/carpetx-mp.git".into(),
        repo: "carpetx-mp".into(),
        thorns: vec![AddedThorn { checkout: format!("{arrangement}/CarpetX"), repo_path: "$2".into() }],
        replaces: vec!["carpetx".into()],
        contact: None,
        libraries: Vec::new(),
        flesh: Vec::new(),
    };
    let replaced = Replaced::Base { repo: "carpetx".into() };
    let by = PkgId::parse("max/carpetx-mp").unwrap();
    let step = Step::Replace { replaced, by, thorns: vec!["CarpetX".into()], flesh: false, explicit: true };
    let plan = Plan { steps: vec![step], ..Plan::default() };
    let meta = InstallationMeta::default();
    place(&base, &meta, vec![fork("CarpetX")], &plan, &mut Vec::new(), &Default::default()).unwrap();
    let err = format!("{:#}", place(&base, &meta, vec![fork("CarpetXMP")], &plan, &mut Vec::new(), &Default::default()).unwrap_err());
    assert!(err.contains("CarpetX/CarpetX") && err.contains("same Arrangement/Thorn"), "{err}");
}

/// The CCL check reads the installation's own thorns from the tree: a base
/// repo no registered package is (looked up as nothing) meets a new thorn's
/// requirement by what its `interface.ccl` says on disk; when it does not,
/// the add is refused, listing what `/providers` offers as `cactup add`
/// lines (a provider on another track named at its version there).
#[test]
fn the_ccl_check_reads_present_thorns_from_the_tree() {
    for meets in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let implements = if meets { "mylib" } else { "other" };
        let (lurl, l) = upstream(tmp.path(), "mylib", &[("MyLib/interface.ccl", &format!("IMPLEMENTS: {implements}\n"))]);
        let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\nINHERITS: mylib\n")]);
        let facts = ThornFacts { implements: Some("z4c".into()), inherits: vec!["mylib".into()], ..ThornFacts::default() };
        let z4c = version_with("1.0.0", &z, &zurl, &[thorn("Z4c", "Z4c", &facts)], "[]", false);
        let mut fake = Fake::new().package("max/z4c", vec![z4c]);
        fake.providers =
            r#"{"providers":[{"package":"et/mylib","version":"mp:1.0.0","arrangement":"MyLib","thorn":"MyLib"}]}"#.into();
        let (base, _) = fake.serve();
        let s = Setup::new(tmp, &section(&lurl, &l, &["MyLib/MyLib"]), &base);
        let result = s.add(&["max/z4c"], yes());
        if meets {
            result.unwrap_or_else(|e| panic!("{e:#}"));
            assert_eq!(s.meta().added.len(), 1);
        } else {
            let err = format!("{:#}", result.unwrap_err());
            assert!(err.contains("Z4c (max/z4c)") && err.contains("mylib"), "{err}");
            assert!(err.contains("cactup add et/mylib/MyLib@mp:1.0.0"), "{err}");
            assert!(s.meta().added.is_empty());
        }
    }
}

/// A remaining added package whose release Cactarium no longer serves
/// (unapproved since, renamed, a commit it cannot find) hides what it
/// depends on: `remove` refuses rather than delete something it may need,
/// naming it and `--no-deps`.
#[test]
fn remove_refuses_while_a_dependent_cannot_be_read() {
    let it = z4c();
    let s = &it.setup;
    s.add(&["max/z4c"], yes()).unwrap();
    it.fake.lock().unwrap().packages.retain(|(p, _)| p != "max/z4c");
    let err = format!("{:#}", s.remove(&["et/carpetx"], remove_yes()).unwrap_err());
    assert!(err.contains("max/z4c") && err.contains("--no-deps"), "{err}");
    assert_eq!(s.meta().added.len(), 2, "nothing removed");
    s.remove(&["et/carpetx"], RemoveFlags { no_deps: true, ..RemoveFlags::default() }).unwrap();
    assert_eq!(s.meta().added.len(), 1);
}

/// `add` beside an added package whose release cannot be read: a plan that
/// moves a present package (or any plan under `--upgrade`) is refused, as
/// that package's ranges are unknown; one that moves nothing goes ahead.
#[test]
fn add_refuses_to_move_packages_beside_one_that_cannot_be_read() {
    let it = admbase("^2.0");
    let s = &it.setup;
    s.add(&["max/solo"], yes()).unwrap();
    it.fake.lock().unwrap().packages.retain(|(p, _)| p != "max/solo");
    let upgrade = AddFlags { upgrade: true, ..AddFlags::default() };
    let err = format!("{:#}", records(s, &["max/app"], &upgrade).unwrap_err());
    assert!(err.contains("max/solo") && err.contains("--no-deps"), "{err}");
    // Nothing moves: a warning, and on.
    records(s, &["max/app2"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    let _ = (&it.a1, &it.a2, &it.log);
}

/// After a dependency gave base package et/admbase another thorn, the base
/// repo and that `[[added]]` entry are one package: a second dependent that
/// needs a base thorn (ADMBase) and one more (ADMMacros) gets them from it,
/// no clash, and the entry holds both added thorns.
#[test]
fn a_second_dependent_of_a_base_package_s_thorns_finds_them() {
    let it = admbase("^1.0");
    let s = &it.setup;
    s.add(&["max/app"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    s.add(&["max/app2"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let added = s.added();
    let admbase = added.iter().find(|(p, _)| p == "et/admbase").unwrap();
    let mut thorns = admbase.1.clone();
    thorns.sort();
    assert_eq!(thorns, ["ADMBase/ADMMacros", "ADMBase/StaticConformal"]);
    assert!(added.iter().any(|(p, _)| p == "max/app2"));
    assert_eq!(s.meta().added.iter().find(|a| a.package == "et/admbase").unwrap().commit, it.a1);
    let _ = (&it.a2, &it.log, &it.fake);
}

/// Thorns of a base package brought in only for a dependency are recorded as
/// brought in, and go once the dependent goes: only those thorns leave the
/// thornlist, the base repo stays. While something needs them they cannot
/// be removed. Named, they stay.
#[test]
fn base_thorns_brought_in_for_a_dependency_go_with_it() {
    let it = admbase("^1.0");
    let s = &it.setup;
    s.add(&["max/app"], yes()).unwrap();
    assert!(!s.meta().added_for["et/admbase"].requested);
    let err = format!("{:#}", s.remove(&["et/admbase"], remove_yes()).unwrap_err());
    assert!(err.contains("max/app@1.0.0"), "{err}");
    s.remove(&["max/app"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.meta().added.is_empty(), "{:?}", s.added());
    let live = std::fs::read_to_string(s.inst.live_thornlist().unwrap()).unwrap();
    assert!(!live.contains("StaticConformal") && live.contains("ADMBase/ADMCoupling"), "{live}");

    // Named, the thorn stays when the package that needed it goes.
    s.add(&["max/app", "et/admbase/StaticConformal"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.meta().added_for["et/admbase"].requested);
    s.remove(&["max/app"], remove_yes()).unwrap();
    assert_eq!(s.added(), [("et/admbase".to_owned(), vec!["ADMBase/StaticConformal".to_owned()])]);
    let _ = (&it.a1, &it.a2, &it.log, &it.fake);
}

/// A Cactarium older than this cactup, with no `/lookup`: `add` says so and
/// names `--no-deps`, which then adds exactly what was named, the base taken
/// as repos of no known package.
#[test]
fn an_older_cactarium_without_lookup() {
    let it = admbase_with("^1.0", |mut f| {
        f.lookup = false;
        f
    });
    let s = &it.setup;
    let err = format!("{:#}", s.add(&["max/app"], yes()).unwrap_err());
    assert!(err.contains("too old to resolve dependencies") && err.contains("--no-deps"), "{err}");
    s.add(&["max/app"], AddFlags { no_deps: true, ..AddFlags::default() }).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(s.added(), [("max/app".to_owned(), vec!["App/App".to_owned()])]);
    let _ = (&it.a1, &it.a2, &it.log, &it.fake);
}

/// The requests `add` and `remove` make on a base of five registered
/// packages: an unmoved present package is read at its commit (one small
/// answer), and whole version lists are asked for only what may move.
#[test]
fn the_requests_add_and_remove_make() {
    let tmp = tempfile::tempdir().unwrap();
    let mut base = String::new();
    let mut fake = Fake::new();
    for name in ["one", "two", "three", "four", "five"] {
        let (url, c) = upstream(tmp.path(), name, &[(&format!("T{name}/interface.ccl"), "IMPLEMENTS: x\n")]);
        base.push_str(&section(&url, &c, &[&format!("Arr{name}/T{name}")]));
        let id = format!("et/{name}");
        fake = fake.package(&id, vec![version("1.0.0", &c, &url, &format!("Arr{name}"), &[&format!("T{name}")], "[]")]);
        fake = fake.pin(&url, &id, Some("1.0.0"));
    }
    let (nurl, n) = upstream(tmp.path(), "new", &[("New/interface.ccl", "IMPLEMENTS: new\n")]);
    let (durl, d) = upstream(tmp.path(), "dep", &[("Dep/interface.ccl", "IMPLEMENTS: dep\n")]);
    let (lurl, l) = upstream(tmp.path(), "lib", &[("Lib/interface.ccl", "IMPLEMENTS: lib\n")]);
    let claims = format!("[{}]", depends("et/lib", "^1.0", &[]));
    let (cactarium, log) = fake
        .package("max/new", vec![version("1.0.0", &n, &nurl, "New", &["New"], "[]")])
        .package("max/dep", vec![version("1.0.0", &d, &durl, "Dep", &["Dep"], &claims)])
        .package("et/lib", vec![version("1.0.0", &l, &lurl, "Lib", &["Lib"], "[]")])
        .serve();
    let s = Setup::new(tmp, &base, &cactarium);
    let count = |specs: &[&str], flags: AddFlags| {
        log.lock().unwrap().clear();
        records(&s, specs, &flags).unwrap_or_else(|e| panic!("{e:#}"));
        counts(&log)
    };
    // [resolve, lookup, whole version lists, one version at a commit, other]
    assert_eq!(count(&["max/new"], AddFlags::default()), [1, 1, 1, 5, 0]);
    assert_eq!(count(&["max/dep"], AddFlags::default()), [1, 1, 2, 5, 0]);
    assert_eq!(count(&["max/dep"], AddFlags { upgrade: true, ..AddFlags::default() }), [1, 1, 7, 0, 0]);
    assert_eq!(count(&["max/dep"], AddFlags { upgrade_all: true, ..AddFlags::default() }), [1, 1, 7, 0, 0]);
    s.add(&["max/new"], yes()).unwrap();
    log.lock().unwrap().clear();
    s.remove(&["max/new"], remove_yes()).unwrap();
    assert_eq!(counts(&log), [0, 1, 0, 6, 0]);
}

/// A command's client carries the header and the stored token under
/// `--risky-allow-untrusted-artifacts`, and neither without it.
#[test]
fn a_commands_client_asks_for_unreviewed_only_under_the_flag() {
    let (base, heads) = crate::fetch::download::test_server::serve_with(2, |_| {
        ("404 Not Found", "api-generation: 1\r\n".into(), b"{}".to_vec())
    });
    crate::cactarium::TEST_TOKENS.lock().unwrap().insert(base.clone(), "cactarium_admin".into());
    let mut s = Setup::new(tempfile::tempdir().unwrap(), "", &base);
    let _ = crate::cactarium::Client::for_command(&s.ctx).unwrap().package_versions("o", "n");
    s.ctx.globals.risky_allow_untrusted_artifacts = true;
    let _ = crate::cactarium::Client::for_command(&s.ctx).unwrap().package_versions("o", "n");
    let heads: Vec<String> = heads.join().unwrap().iter().map(|h| h.to_ascii_lowercase()).collect();
    assert!(!heads[0].contains("include-unreviewed") && !heads[0].contains("authorization"), "{}", heads[0]);
    assert!(heads[1].contains("x-cactarium-include-unreviewed: true"), "{}", heads[1]);
    assert!(heads[1].contains("authorization: bearer cactarium_admin"), "{}", heads[1]);
}

/// An installed package that Cactarium no longer serves (a release awaiting review, read
/// without `--risky-allow-untrusted-artifacts`) is named by the version recorded for it, not by
/// its commit, in every line that names it.
#[test]
fn an_installed_package_not_served_is_named_by_its_recorded_version() {
    let it = z4c();
    let s = &it.setup;
    s.add(&["max/z4c"], yes()).unwrap();
    it.fake.lock().unwrap().packages.retain(|(p, _)| p != "max/z4c");
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let list = thornlist::parse(&pristine).unwrap();
    let meta = s.meta();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    let names: Vec<&str> = meta.added.iter().map(|a| a.package.as_str()).collect();
    let mut reg = super::super::resolution::Registry::new(&client, names);
    let set = super::super::resolution::present(&mut reg, &s.inst.root, &list, &meta.added, None, &Default::default());
    reg.finish();
    let set = set.unwrap_or_else(|e| panic!("{e:#}"));
    let z = set.present.iter().find(|p| p.release.id.as_str() == "max/z4c").expect("present");
    assert_eq!(z.release.label(), "max/z4c@1.0.0");
    assert_eq!(set.unread.len(), 1);
}

/// The first request a command makes, lowercased, to a server that answers every request 404
/// (empty when it made none). Run against `s` pointed at that server, with a token stored.
fn first_request(s: &Setup, run: impl FnOnce(&Setup)) -> String {
    let (base, heads) = crate::fetch::download::test_server::serve_with(1, |_| {
        ("404 Not Found", "api-generation: 1\r\n".into(), b"{}".to_vec())
    });
    crate::cactarium::TEST_TOKENS.lock().unwrap().insert(base.clone(), "cactarium_admin".into());
    let to = base.clone();
    s.ctx
        .db
        .update(move |d| {
            d.set_knob(crate::cactarium::URL_KNOB, to);
            Ok(())
        })
        .unwrap();
    run(s);
    // Lets the server finish when the command made no request.
    let _ = std::net::TcpStream::connect(base.trim_start_matches("http://"));
    heads.join().unwrap().into_iter().next().unwrap_or_default().to_ascii_lowercase()
}

/// Every command that asks Cactarium follows `--risky-allow-untrusted-artifacts`: its requests
/// carry the header and the token with it, and neither without it. Flipping any call site
/// back to an anonymous client fails here.
#[test]
fn every_command_that_asks_cactarium_follows_the_flag() {
    use crate::args::{ClaimsCommand, OverrideAction};
    let mut it = z4c();
    it.setup.add(&["max/z4c"], yes()).unwrap();
    let s = &mut it.setup;
    let mut sim = cactup_ecosystem_common::sim::SimFile::from_toml(&crate::commands::import::tests::stamped("max/bbh@1.2.0")).unwrap();
    sim.sim.cactarium = Some("max/bbh@1.2.0".parse().unwrap());
    sim.inputs.push(cactup_ecosystem_common::sim::Input {
        knob: "K".into(),
        description: None,
        url: None,
        sha256: None,
        unpack: false,
        path: None,
        fixups: vec![],
        repo_file: Some("in.txt".into()),
    });
    type Run = Box<dyn Fn(&Setup)>;
    let commands: Vec<(&str, Run)> = vec![
        ("add", Box::new(|s| drop(s.add(&["max/z4c"], yes())))),
        ("remove", Box::new(|s| drop(s.remove(&["max/z4c"], remove_yes())))),
        ("claims check", Box::new(|s| drop(crate::commands::claims::dispatch(&s.ctx, ClaimsCommand::Check)))),
        ("claims problems", Box::new(|s| drop(crate::commands::claims::problems(&s.ctx, &s.inst, &s.meta())))),
        ("claims warnings", Box::new(|s| drop(crate::commands::claims::warn_problems(&s.ctx, &s.inst)))),
        (
            "claims override",
            Box::new(|s| {
                let action = OverrideAction::Drop { claim: "max/z4c:dependency:et/carpetx".into(), note: None };
                drop(crate::commands::claims::dispatch(&s.ctx, ClaimsCommand::Override { remove: None, action: Some(action) }))
            }),
        ),
        ("import", Box::new(|s| drop(crate::commands::import::download_published(&s.ctx, "max/bbh")))),
        ("import inputs", Box::new(move |s| drop(crate::commands::import::repo_source(&s.ctx, &sim, &[])))),
        ("stack", Box::new(|s| drop(crate::cactarium::stack_release_text(&s.ctx, "o/s@head")))),
    ];
    for flag in [false, true] {
        s.ctx.globals.risky_allow_untrusted_artifacts = flag;
        for (name, run) in &commands {
            let head = first_request(s, |s| run(s));
            assert!(head.starts_with("get ") || head.starts_with("post "), "{name} made no request: {head:?}");
            assert_eq!(head.contains("x-cactarium-include-unreviewed: true"), flag, "{name}, flag {flag}: {head}");
            assert_eq!(head.contains("authorization: bearer cactarium_admin"), flag, "{name}, flag {flag}: {head}");
        }
    }
}

/// The live thornlist is read before the prompt; hand edits made while it
/// waits are not overwritten: the check under the lock sees them.
#[test]
fn hand_edits_made_while_the_prompt_waits_are_caught() {
    let it = z4c();
    let s = &it.setup;
    let start = Start::read(&s.ctx, false, Doing::Adding).unwrap();
    let live = s.inst.live_thornlist().unwrap();
    let text = std::fs::read_to_string(&live).unwrap();
    std::fs::write(&live, format!("{text}\n!TARGET = $ROOT/arrangements\n!TYPE = ignore\n!CHECKOUT = Mine/Mine\n")).unwrap();
    let locked = start.inst.locked().unwrap();
    let err = format!("{:#}", start.recheck(&locked).unwrap_err());
    assert!(err.contains("changed while"), "{err}");
    let _ = (&it.z, &it.c, &it.log, &it.fake);
}

/// The check that a replaced thorn keeps the base's `Arrangement/Thorn` runs
/// before anything is confirmed: `--dry-run` already refuses.
#[test]
fn a_replaced_checkout_is_checked_before_the_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let (curl, c) = upstream(tmp.path(), "carpetx", &[("CarpetX/interface.ccl", "IMPLEMENTS: carpetx\n")]);
    let (furl, f) = upstream(tmp.path(), "carpetx-mp", &[("CarpetX/interface.ccl", "IMPLEMENTS: carpetx\n")]);
    let (cactarium, _) = Fake::new()
        .package("max/carpetx-mp", vec![version("1.0.0", &f, &furl, "CarpetXMP", &["CarpetX"], "[]")])
        .serve();
    let s = Setup::new(tmp, &section(&curl, &c, &["CarpetX/CarpetX"]), &cactarium);
    let flags = AddFlags { replace: true, dry_run: true, ..AddFlags::default() };
    let err = format!("{:#}", s.add(&["max/carpetx-mp"], flags).unwrap_err());
    assert!(err.contains("same Arrangement/Thorn"), "{err}");
}

/// No `/providers` on an older Cactarium: the unmet requirement is still the
/// refusal, saying Cactarium cannot list what provides it.
#[test]
fn an_older_cactarium_without_providers_keeps_the_refusal() {
    let tmp = tempfile::tempdir().unwrap();
    let (lurl, l) = upstream(tmp.path(), "mylib", &[("MyLib/interface.ccl", "IMPLEMENTS: other\n")]);
    let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\nINHERITS: mylib\n")]);
    let facts = ThornFacts { implements: Some("z4c".into()), inherits: vec!["mylib".into()], ..ThornFacts::default() };
    let z4c = version_with("1.0.0", &z, &zurl, &[thorn("Z4c", "Z4c", &facts)], "[]", false);
    let mut fake = Fake::new().package("max/z4c", vec![z4c]);
    fake.providers_endpoint = false;
    let (base, _) = fake.serve();
    let s = Setup::new(tmp, &section(&lurl, &l, &["MyLib/MyLib"]), &base);
    let err = format!("{:#}", s.add(&["max/z4c"], yes()).unwrap_err());
    assert!(err.contains("Z4c (max/z4c)") && err.contains("mylib"), "{err}");
    assert!(err.contains("too old to list providers"), "{err}");
    assert!(!err.contains("no registered package provides it"), "{err}");
}

/// Without its pristine thornlist an installation has nothing to add to.
#[test]
fn add_needs_the_pristine_thornlist() {
    let it = z4c();
    let s = &it.setup;
    std::fs::remove_file(s.inst.source_thornlist()).unwrap();
    let err = format!("{:#}", s.add(&["max/z4c"], yes()).unwrap_err());
    assert!(err.contains("no pristine thornlist to add to"), "{err}");
    let _ = (&it.z, &it.c, &it.log, &it.fake);
}

/// A base that follows branches: twenty registered repos at commits no
/// version froze. They are read in one batch request (the server reads them
/// now, 12 requests a minute), never one by one; under `--no-deps` not at
/// all. `add`, `add --no-deps`, `add --upgrade` and `remove` all go
/// through, each with a handful of requests.
#[test]
fn a_base_at_unfrozen_commits_is_read_in_one_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let mut base = String::new();
    let mut fake = Fake::new();
    for i in 0..20 {
        let name = format!("r{i}");
        let (url, c) = upstream(tmp.path(), &name, &[(&format!("T{i}/interface.ccl"), "IMPLEMENTS: x\n")]);
        base.push_str(&section(&url, &c, &[&format!("Arr{i}/T{i}")]));
        let id = format!("et/{name}");
        // Its one version froze another commit.
        let other = format!("{:040x}", i + 1);
        fake = fake.package(&id, vec![version("1.0.0", &other, &url, &format!("Arr{i}"), &[&format!("T{i}")], "[]")]);
        fake = fake.pin(&url, &id, None);
    }
    let (nurl, n) = upstream(tmp.path(), "new", &[("New/interface.ccl", "IMPLEMENTS: new\n")]);
    let (cactarium, log, fake) =
        fake.package("max/new", vec![version("1.0.0", &n, &nurl, "New", &["New"], "[]")]).serve_shared();
    let s = Setup::new(tmp, &base, &cactarium);
    let count = |specs: &[&str], flags: AddFlags| {
        log.lock().unwrap().clear();
        records(&s, specs, &flags).unwrap_or_else(|e| panic!("{e:#}"));
        counts(&log)
    };
    // [resolve, lookup, whole version lists, one version at a commit, other]
    // [resolve, lookup, whole version lists, one version at a commit, other (the batch)]
    assert_eq!(count(&["max/new"], AddFlags::default()), [1, 1, 1, 0, 1]);
    assert_eq!(count(&["max/new"], AddFlags { no_deps: true, ..AddFlags::default() }), [1, 1, 1, 0, 0]);
    assert_eq!(count(&["max/new"], AddFlags { upgrade: true, ..AddFlags::default() }), [1, 1, 21, 0, 1]);
    s.add(&["max/new"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    log.lock().unwrap().clear();
    s.remove(&["max/new"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    // max/new read at its commit, the base in one batch.
    assert_eq!(counts(&log), [0, 1, 0, 1, 1]);
    assert_eq!(fake.lock().unwrap().unfrozen_reads, 0);
}

/// A base package's frozen release that Cactarium will not serve now
/// (403: not served to this caller; 429 alike) leaves its dependencies
/// unknown, no failure; replacing it is no move beside an unread package.
#[test]
fn a_base_read_refused_leaves_it_unchecked() {
    let it = admbase("^1.0");
    let s = &it.setup;
    it.fake.lock().unwrap().refuse_at.push(it.a1.clone());
    let replace = AddFlags { replace: true, ..AddFlags::default() };
    let got = records(s, &["et/admbase/StaticConformal@2.0.0"], &replace).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(record_of(&got, "et/admbase").0, "2.0.0");
    records(s, &["max/solo"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    let _ = (&it.a2, &it.log);
}

/// A registered fork in the base (max/carpet-mp, which stands in for
/// et/carpet) given CarpetSlab for max/d: the extension is held through the
/// fork, so it cannot be removed while max/d needs it, nor swept as an
/// orphan when something unrelated goes.
#[test]
fn a_base_extension_held_through_a_fork_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let (furl, f) = upstream(
        tmp.path(),
        "carpet-mp",
        &[("Carpet/interface.ccl", "IMPLEMENTS: driver\n"), ("CarpetSlab/interface.ccl", "IMPLEMENTS: hyperslab\n")],
    );
    let (durl, d) = upstream(tmp.path(), "d", &[("D/interface.ccl", "IMPLEMENTS: d\n")]);
    let (surl, so) = upstream(tmp.path(), "solo", &[("Solo/interface.ccl", "IMPLEMENTS: solo\n")]);
    let fork = r#"[{"kind":"fork-of","target":"et/carpet","range":"=2026.5.0","note":null}]"#;
    let claims = format!("[{}]", depends("et/carpet", "^2026.5", &["CarpetSlab"]));
    let (cactarium, _) = Fake::new()
        .package("max/carpet-mp", vec![version("1.0.0", &f, &furl, "Carpet", &["Carpet", "CarpetSlab"], fork)])
        .package("et/carpet", vec![version("2026.5.0", &"e".repeat(40), "https://x/carpet.git", "Carpet", &["Carpet", "CarpetSlab"], "[]")])
        .package("max/d", vec![version("1.0.0", &d, &durl, "D", &["D"], &claims)])
        .package("max/solo", vec![version("1.0.0", &so, &surl, "Solo", &["Solo"], "[]")])
        .pin(&furl, "max/carpet-mp", None)
        .serve();
    let s = Setup::new(tmp, &section(&furl, &f, &["Carpet/Carpet"]), &cactarium);
    s.add(&["max/d"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.added().contains(&("max/carpet-mp".to_owned(), vec!["Carpet/CarpetSlab".to_owned()])), "{:?}", s.added());
    assert!(!s.meta().added_for["max/carpet-mp"].requested);

    let err = format!("{:#}", s.remove(&["max/carpet-mp"], remove_yes()).unwrap_err());
    assert!(err.contains("max/d@1.0.0"), "{err}");
    s.add(&["max/solo"], yes()).unwrap();
    s.remove(&["max/solo"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.added().iter().any(|(p, _)| p == "max/carpet-mp"), "{:?}", s.added());
    // max/d gone, nothing needs it: it goes along.
    s.remove(&["max/d"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.added().is_empty(), "{:?}", s.added());
}

/// Naming a thorn a dependency brought into a base package keeps it as
/// named, with nothing installed again: it stays when that dependency goes.
#[test]
fn naming_a_brought_in_base_thorn_keeps_it() {
    let it = admbase("^1.0");
    let s = &it.setup;
    s.add(&["max/app"], yes()).unwrap();
    assert!(!s.meta().added_for["et/admbase"].requested);
    s.add(&["et/admbase/StaticConformal"], AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.meta().added_for["et/admbase"].requested);
    s.remove(&["max/app"], remove_yes()).unwrap();
    assert_eq!(s.added(), [("et/admbase".to_owned(), vec!["ADMBase/StaticConformal".to_owned()])]);
    let _ = (&it.a1, &it.a2, &it.log, &it.fake);
}

/// After `inst refetch` moves the base to another commit, the thorns a
/// dependency gave its package follow it, and are still that package: a
/// further dependent of it is no clash, and the entry grows at the new
/// commit.
#[test]
fn a_base_extension_follows_a_refetch_of_the_base() {
    let it = admbase("^1.0");
    let s = &it.setup;
    s.add(&["max/app"], yes()).unwrap();
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let moved = pristine.replace(&it.a1, &it.a2);
    let file = s.inst.root.join("moved.th");
    std::fs::write(&file, &moved).unwrap();
    let args = match <crate::args::Args as clap::Parser>::try_parse_from(["cactup", "inst", "refetch", file.to_str().unwrap()])
        .unwrap()
        .command
    {
        crate::args::Commands::Inst(crate::args::InstallationCommand::Refetch(args)) => args,
        other => panic!("{other:?}"),
    };
    super::super::refetch::dispatch(&s.ctx, args).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(crate::fetch::FetchState::read(&s.inst.root).unwrap().unwrap().repos["admbase"].head, it.a2);

    s.add(&["max/app3"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let meta = s.meta();
    let admbase = meta.added.iter().find(|a| a.package == "et/admbase").unwrap();
    let mut thorns: Vec<&str> = admbase.thorns.iter().map(|t| t.checkout.as_str()).collect();
    thorns.sort();
    assert_eq!(thorns, ["ADMBase/ADMMacros", "ADMBase/StaticConformal"]);
    assert_eq!(admbase.commit, it.a2);
    assert!(s.inst.root.join("Cactus/arrangements/ADMBase/ADMMacros/interface.ccl").is_file());
    let _ = (&it.log, &it.fake);
}

/// One commit frozen by two versions (two tracks): the present release is
/// the one the lookup names, read lazily or in full alike.
#[test]
fn the_lookup_s_version_picks_the_track() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, c) = upstream(tmp.path(), "two", &[("T/interface.ccl", "IMPLEMENTS: t\n")]);
    let (default, mp) = (version("2.0.0", &c, &url, "Arr", &["T"], "[]"), version("mp:2.0.0", &c, &url, "Arr", &["T"], "[]"));
    let (cactarium, _, fake) =
        Fake::new().package("et/two", vec![default.clone(), mp.clone()]).pin(&url, "et/two", None).serve_shared();
    let s = Setup::new(tmp, &section(&url, &c, &["Arr/T"]), &cactarium);
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let list = thornlist::parse(&pristine).unwrap();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    for full in [false, true] {
        // Either way the first release found at the commit is the other
        // track's: the one read at the commit (the fake answers the last
        // listed), or the first in the whole list.
        fake.lock().unwrap().packages[0].1 =
            if full { vec![mp.clone(), default.clone()] } else { vec![default.clone(), mp.clone()] };
        let mut reg = super::super::resolution::Registry::new(&client, []);
        let options = super::super::resolution::PresentOptions { full, ..Default::default() };
        let set = super::super::resolution::present(&mut reg, &s.inst.root, &list, &[], None, &options);
        reg.finish();
        let set = set.unwrap_or_else(|e| panic!("{e:#}"));
        let labels: Vec<String> = set.present.iter().map(|p| p.release.label()).collect();
        assert_eq!(labels, ["et/two@2.0.0"], "full: {full}");
    }
}

/// A write that only gives an `added-for` entry more (more thorns at the
/// same commit) edits it: what a newer binary put in it stays.
#[test]
fn an_entry_edited_at_the_same_commit_keeps_unknown_keys() {
    let it = admbase("^1.0");
    let s = &it.setup;
    s.add(&["max/app"], yes()).unwrap();
    let path = s.inst.cactup_dir().join("installation.toml");
    let mut doc: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["added-for"]["et/admbase"].as_table_mut().unwrap().insert("cactup-future".into(), toml::Value::Integer(1));
    std::fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();
    s.add(&["max/app2"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let doc: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(doc["added-for"]["et/admbase"].get("cactup-future").is_some(), "{doc:?}");
    let _ = (&it.a1, &it.a2, &it.log, &it.fake);
}

/// A base that follows a branch (its admbase section unpinned): more thorns
/// of its package live at the commit the clone is at on disk. They are linked
/// in; the clone is neither fetched nor moved, even with new commits
/// upstream.
#[test]
fn more_thorns_of_an_unpinned_base_clone_are_linked_where_it_is() {
    // Its clone on its branch, at the tip: a2.
    let it = admbase_full("^1.0", false, true, |f| f);
    let s = &it.setup;
    let up = s.inst.root.parent().unwrap().join("up/admbase");
    let a3 = crate::fetch::tests::commit_files(&up, &[("ADMBase/interface.ccl", "IMPLEMENTS: admbase\n"), ("NEW", "3\n")]).to_string();
    s.add(&["max/app3"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let state = crate::fetch::FetchState::read(&s.inst.root).unwrap().unwrap();
    assert_eq!(state.repos["admbase"].head, it.a2, "not moved to {a3}");
    assert!(s.inst.root.join("Cactus/arrangements/ADMBase/ADMMacros/interface.ccl").is_file());
    let meta = s.meta();
    let admbase = meta.added.iter().find(|a| a.package == "et/admbase").unwrap();
    assert_eq!(admbase.commit, it.a2);
    let _ = (&it.a1, &it.log, &it.fake);
}

/// Two base clones of one package: only the first is the package; thorns
/// added to the second stay present, as the added package they are.
#[test]
fn thorns_added_to_a_second_base_clone_of_a_package_stay_present() {
    let tmp = tempfile::tempdir().unwrap();
    let (u1, c1) = upstream(tmp.path(), "one", &[("A/interface.ccl", "IMPLEMENTS: a\n")]);
    let (u2, c2) = upstream(tmp.path(), "two", &[("B/interface.ccl", "IMPLEMENTS: b\n"), ("C/interface.ccl", "IMPLEMENTS: c\n")]);
    let (cactarium, _) = Fake::new()
        .package("et/x", vec![version("1.0.0", &c1, &u1, "Arr", &["A"], "[]"), version("0.9.0", &c2, &u2, "Arr", &["B", "C"], "[]")])
        .pin(&u1, "et/x", None)
        .pin(&u2, "et/x", None)
        .serve();
    let base = format!("{}{}", section(&u1, &c1, &["Arr/A"]), section(&u2, &c2, &["Arr/B"]));
    let s = Setup::new(tmp, &base, &cactarium);
    let added = AddedPackage {
        package: "et/x".into(),
        version: "0.9.0".into(),
        tag: None,
        commit: c2.clone(),
        url: u2.clone(),
        repo: "two".into(),
        thorns: vec![AddedThorn { checkout: "Arr/C".into(), repo_path: "$2".into() }],
        replaces: Vec::new(),
        contact: None,
        libraries: Vec::new(),
        flesh: Vec::new(),
    };
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let list = thornlist::parse(&pristine).unwrap();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    let mut reg = super::super::resolution::Registry::new(&client, []);
    let options = super::super::resolution::PresentOptions::default();
    let set = super::super::resolution::present(&mut reg, &s.inst.root, &list, &[added], None, &options);
    reg.finish();
    let set = set.unwrap_or_else(|e| panic!("{e:#}"));
    let enabled: Vec<String> = set.present.iter().flat_map(|p| p.enabled.iter().cloned()).collect();
    assert!(enabled.contains(&"C".to_owned()), "{enabled:?}");
}

/// A base at a commit no version froze: naming a thorn of its package reads
/// the package there (one read) and keeps it at its commit, `--no-deps` or
/// not; never an upgrade to the newest release.
#[test]
fn a_named_thorn_of_an_unfrozen_base_package_keeps_its_commit() {
    for no_deps in [true, false] {
        let it = admbase_unfrozen("^1.0");
        let s = &it.setup;
        s.add(&["et/admbase/StaticConformal"], AddFlags { no_deps, ..AddFlags::default() }).unwrap_or_else(|e| panic!("{e:#}"));
        let meta = s.meta();
        let admbase = meta.added.iter().find(|a| a.package == "et/admbase").unwrap();
        assert_eq!((admbase.commit.as_str(), admbase.replaces.len()), (it.a1.as_str(), 0), "no_deps {no_deps}");
        assert_eq!(admbase.thorns.iter().map(|t| t.checkout.as_str()).collect::<Vec<_>>(), ["ADMBase/StaticConformal"]);
        // Read once: singly under --no-deps (no batch of the base), else in the batch.
        let f = it.fake.lock().unwrap();
        assert_eq!(f.unfrozen_reads + f.batch_fetches, 1, "no_deps {no_deps}");
        drop(f);
        let _ = (&it.a2, &it.log);
    }
}

/// A dependency on a thorn the unfrozen base package has but does not
/// enable: read there, the thorn added, no false "pass --upgrade".
#[test]
fn a_needed_thorn_of_an_unfrozen_base_package_is_found() {
    let it = admbase_unfrozen("^1.0");
    let s = &it.setup;
    s.add(&["max/app"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let added = s.added();
    assert!(added.contains(&("et/admbase".to_owned(), vec!["ADMBase/StaticConformal".to_owned()])), "{added:?}");
    let _ = (&it.a1, &it.a2, &it.log);
}

/// The read refused (429: the server's limit on reads at unfrozen commits):
/// a plan that hinges on the package refuses, naming it and `--no-deps`;
/// one that does not touch it goes ahead.
#[test]
fn an_unfrozen_base_package_the_server_will_not_read_refuses_what_hinges_on_it() {
    let it = admbase_unfrozen("^1.0");
    let s = &it.setup;
    it.fake.lock().unwrap().always_limited = true;
    super::super::resolution::TEST_READ_WINDOW.with(|w| w.set(Some(std::time::Duration::from_millis(200))));
    let err = format!("{:#}", s.add(&["max/app"], yes()).unwrap_err());
    assert!(err.contains("et/admbase#") && err.contains("its read limit held after waiting"), "{err}");
    assert!(err.contains("Pass --upgrade") && err.contains("--no-deps"), "{err}");
    s.add(&["max/solo"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let _ = (&it.a1, &it.a2, &it.log);
}

/// A registered fork in the base at a commit no version froze, which shares
/// thorn names with what the plan brings in: read there, it stands in for
/// et/carpet, which max/d needs; et/carpet is not added beside it.
#[test]
fn an_unfrozen_base_fork_stands_in() {
    let tmp = tempfile::tempdir().unwrap();
    let (furl, f) = upstream(
        tmp.path(),
        "carpet-mp",
        &[("Carpet/interface.ccl", "IMPLEMENTS: driver\n"), ("CarpetSlab/interface.ccl", "IMPLEMENTS: hyperslab\n")],
    );
    let (durl, d) = upstream(tmp.path(), "d", &[("D/interface.ccl", "IMPLEMENTS: d\n")]);
    let fork = r#"[{"kind":"fork-of","target":"et/carpet","range":"=2026.5.0","note":null}]"#;
    let claims = format!("[{}]", depends("et/carpet", "^2026.5", &["CarpetSlab"]));
    let mut fake = Fake::new()
        .package("max/carpet-mp", vec![version("1.0.0", &"9".repeat(40), &furl, "Carpet", &["Carpet", "CarpetSlab"], fork)])
        .package("et/carpet", vec![version("2026.5.0", &"e".repeat(40), "https://x/carpet.git", "Carpet", &["Carpet", "CarpetSlab"], "[]")])
        .package("max/d", vec![version("1.0.0", &d, &durl, "D", &["D"], &claims)])
        .pin(&furl, "max/carpet-mp", None);
    let read = version(&f, &f, &furl, "Carpet", &["Carpet", "CarpetSlab"], fork);
    fake.snapshots.push(("max/carpet-mp".to_owned(), f.clone(), read));
    let (cactarium, _) = fake.serve();
    let s = Setup::new(tmp, &section(&furl, &f, &["Carpet/Carpet"]), &cactarium);
    s.add(&["max/d"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let added = s.added();
    assert!(added.contains(&("max/carpet-mp".to_owned(), vec!["Carpet/CarpetSlab".to_owned()])), "{added:?}");
    assert!(!added.iter().any(|(p, _)| p == "et/carpet"), "{added:?}");
}

/// The requests on a base of twenty registered repos at commits no version
/// froze, when a dependency names a thorn of one of them that is not
/// enabled: the whole base is read in one batch request; a Cactarium without
/// batch reads reads that one package at its commit, nothing else.
#[test]
fn a_dependency_on_an_unfrozen_base_package_reads_it_in_the_batch_or_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let mut base = String::new();
    let mut fake = Fake::new();
    let mut r0 = (String::new(), String::new());
    for i in 0..20 {
        let name = format!("r{i}");
        let (url, c) = upstream(tmp.path(), &name, &[(&format!("T{i}/interface.ccl"), "IMPLEMENTS: x\n"), (&format!("U{i}/interface.ccl"), "IMPLEMENTS: u\n")]);
        base.push_str(&section(&url, &c, &[&format!("Arr{i}/T{i}")]));
        let id = format!("et/{name}");
        let other = format!("{:040x}", i + 1);
        let thorns = [format!("T{i}"), format!("U{i}")];
        let thorns: Vec<&str> = thorns.iter().map(String::as_str).collect();
        fake = fake.package(&id, vec![version("1.0.0", &other, &url, &format!("Arr{i}"), &thorns, "[]")]);
        fake.snapshots.push((id.clone(), c.clone(), version(&c, &c, &url, &format!("Arr{i}"), &thorns, "[]")));
        fake = fake.pin(&url, &id, None);
        if i == 0 {
            r0 = (url, c);
        }
    }
    let (durl, d) = upstream(tmp.path(), "dep", &[("Dep/interface.ccl", "IMPLEMENTS: dep\n")]);
    let claims = format!("[{}]", depends("et/r0", ">=0.0.0", &["U0"]));
    let (cactarium, log, fake) =
        fake.package("max/dep", vec![version("1.0.0", &d, &durl, "Dep", &["Dep"], &claims)]).serve_shared();
    let s = Setup::new(tmp, &base, &cactarium);
    log.lock().unwrap().clear();
    let got = records(&s, &["max/dep"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(record_of(&got, "et/r0").2, ["Arr0/U0"]);
    // [resolve, lookup, whole version lists, one version at a commit, other]:
    // the whole base in one batch request.
    assert_eq!(counts(&log), [1, 1, 1, 0, 1]);
    let f = fake.lock().unwrap();
    assert_eq!((f.unfrozen_reads, f.batch_fetches), (0, 20));
    drop(f);
    // A Cactarium without batch reads: only the package the plan depends on,
    // read singly (after the batch request it does not know).
    fake.lock().unwrap().batch = false;
    log.lock().unwrap().clear();
    let got = records(&s, &["max/dep"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(record_of(&got, "et/r0").2, ["Arr0/U0"]);
    assert_eq!(counts(&log), [1, 1, 1, 1, 1]);
    assert_eq!(fake.lock().unwrap().unfrozen_reads, 1);
    let _ = r0;
}

/// A base clone moved by hand (a clean `git pull` in a base that follows
/// its branch) is read where its HEAD is, not where the fetch record says:
/// thorns are never linked in from a commit the clone is no longer at.
#[test]
fn a_base_clone_moved_by_hand_is_read_at_its_head() {
    let it = admbase_full("^1.0", false, true, |f| f);
    let s = &it.setup;
    // Upstream moves on, without ADMMacros; the clone is pulled there, and
    // the fetch record left as it was.
    let up = s.inst.root.parent().unwrap().join("up/admbase");
    let a3 = crate::fetch::tests::commit_files(
        &up,
        &[("ADMBase/interface.ccl", "IMPLEMENTS: admbase\n"), ("ADMCoupling/interface.ccl", "IMPLEMENTS: admcoupling\n")],
    )
    .to_string();
    let state = s.inst.root.join(".cactup/fetch-state.toml");
    let recorded = std::fs::read_to_string(&state).unwrap();
    let live = std::fs::read_to_string(s.inst.live_thornlist().unwrap()).unwrap();
    let list = thornlist::parse(&live).unwrap();
    let mut progress = prodash::tree::Root::new().add_child("plan");
    let plan = crate::fetch::plan(&list, &s.inst.root, &mut progress).unwrap();
    let report = crate::fetch::execute(&plan, &s.inst.root).unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    std::fs::write(&state, recorded).unwrap();
    assert_eq!(crate::fetch::FetchState::read(&s.inst.root).unwrap().unwrap().repos["admbase"].head, it.a2);
    assert_eq!(super::clone_head(&s.inst.root, "Cactus", "admbase").as_deref(), Some(a3.as_str()));

    // ADMMacros is not at HEAD: no link into it; the add says why.
    let result = s.add(&["max/app3"], yes());
    let macros = s.inst.root.join("Cactus/arrangements/ADMBase/ADMMacros/interface.ccl");
    match result {
        Ok(()) => assert!(macros.is_file(), "a link to a thorn the clone does not have"),
        Err(e) => {
            let e = format!("{e:#}");
            assert!(e.contains(&a3[..12]), "{e}");
            assert!(!macros.exists());
        }
    }
    let _ = (&it.a1, &it.log, &it.fake);
}

/// The fetch checked before the prompt is made again under the lock and
/// must be the same: a clone edited, or taken away, while the question
/// waited refuses as the installation having changed.
#[test]
fn clones_changed_while_the_prompt_waits_are_caught() {
    let it = admbase("^1.0");
    let s = &it.setup;
    s.add(&["max/solo"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let start = Start::read(&s.ctx, false, Doing::Adding).unwrap();
    let base = thornlist::parse(&start.pristine).unwrap();
    let added: Vec<AddedPackage> = start.meta.added.clone();
    let composed = super::compose(&start.pristine, &start.meta.added).unwrap();
    let checked = super::fetch_plan_for(&start.inst, &composed, &base, &added).unwrap();
    let again = super::fetch_plan_for(&start.inst, &composed, &base, &added).unwrap();
    super::same_fetch(&checked, &again).unwrap();

    // Edited: the fetch would now leave it alone.
    let solo = s.inst.root.join("Cactus/repos/solo");
    std::fs::write(solo.join("Solo/interface.ccl"), "IMPLEMENTS: solo\n# edited\n").unwrap();
    let err = super::fetch_plan_for(&start.inst, &composed, &base, &added).map_err(super::clones_changed).unwrap_err();
    assert!(format!("{err:#}").contains("changed while the plan waited"), "{err:#}");

    // The same through `add`, the clone edited while its prompt waits.
    std::fs::write(solo.join("Solo/interface.ccl"), "IMPLEMENTS: solo\n").unwrap();
    super::AFTER_PROMPT.with(|h| {
        h.set(Some(|root| std::fs::write(root.join("Cactus/repos/solo/Solo/interface.ccl"), "# edited\n").unwrap()))
    });
    let err = s.add(&["max/solo"], yes());
    super::AFTER_PROMPT.with(|h| h.set(None));
    let err = format!("{:#}", err.unwrap_err());
    assert!(err.contains("changed while the plan waited"), "{err}");
    std::fs::write(solo.join("Solo/interface.ccl"), "IMPLEMENTS: solo\n").unwrap();

    // Taken away: the fetch would now clone it.
    std::fs::remove_dir_all(&solo).unwrap();
    let now = super::fetch_plan_for(&start.inst, &composed, &base, &added).unwrap();
    let err = super::same_fetch(&checked, &now).unwrap_err();
    assert!(format!("{err:#}").contains("changed while the plan waited"), "{err:#}");
    let _ = (&it.a1, &it.a2, &it.log, &it.fake);
}

/// Ctrl-C landing after the fetch's git work, with nothing left queued, and
/// before its link pass fails the add ("Interrupted"): the thorn is not
/// linked, and neither the list nor the record is written. The interrupt
/// flag is the process's: run in a child of its own.
#[test]
fn an_interrupt_between_the_fetch_and_its_links_fails_the_add() {
    const NAME: &str = "commands::add::deps_tests::an_interrupt_between_the_fetch_and_its_links_fails_the_add";
    if std::env::var_os("CACTUP_TEST_INTERRUPT_CHILD").is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([NAME, "--exact", "--test-threads=1"])
            .env("CACTUP_TEST_INTERRUPT_CHILD", "1")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && said.contains("1 passed"), "{said}{}", String::from_utf8_lossy(&out.stderr));
        return;
    }
    let it = admbase("^1.0");
    let s = &it.setup;
    let live = s.inst.live_thornlist().unwrap();
    let list = std::fs::read_to_string(&live).unwrap();
    let added = s.added();
    crate::fetch::AFTER_POOL.with(|h| {
        h.set(Some(|| gix::interrupt::IS_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst)))
    });
    let result = s.add(&["max/solo"], yes());
    crate::fetch::AFTER_POOL.with(|h| h.set(None));
    let err = result.unwrap_err();
    assert_eq!(format!("{err:#}"), "Interrupted");
    // Cloned, and not linked.
    assert!(s.inst.root.join("Cactus/repos/solo/Solo/interface.ccl").is_file());
    assert!(std::fs::symlink_metadata(s.inst.root.join("Cactus/arrangements/Solo")).is_err());
    assert_eq!(std::fs::read_to_string(&live).unwrap(), list);
    assert_eq!(s.added(), added);
    let _ = (&it.a1, &it.a2, &it.log, &it.fake);
}

/// `n` packages, each read at a commit of its own in the fake (`fake` made
/// as `change` says), read up front by one registry: the batch requests and
/// rounds it took, and how many it read.
fn prefetch_on(n: usize, change: impl FnOnce(&mut Fake)) -> (usize, usize, usize, Shared) {
    let (requests, rounds, read, _, fake) = prefetch_waits(n, change);
    (requests, rounds, read, fake)
}

/// [`prefetch_on`], with the waits between rounds.
fn prefetch_waits(n: usize, change: impl FnOnce(&mut Fake)) -> (usize, usize, usize, Vec<std::time::Duration>, Shared) {
    let mut fake = Fake::new();
    let mut items = Vec::new();
    for i in 0..n {
        let (id, c) = (format!("et/p{i:03}"), format!("{:040x}", i + 1));
        fake.snapshots.push((id.clone(), c.clone(), version(&c, &c, "https://x/p.git", "P", &["P"], "[]")));
        items.push((crate::deps::PkgId::parse(&id).unwrap(), c));
    }
    change(&mut fake);
    let (base, _, fake) = fake.serve_shared();
    let client = crate::cactarium::Client::at(&base);
    let mut reg = super::super::resolution::Registry::new(&client, []);
    reg.prefetch(&items).unwrap();
    let read = items.iter().filter(|(id, c)| reg.batch_read(id, c)).count();
    let (requests, rounds, pauses) = (reg.batch_requests, reg.batch_rounds, reg.pauses.clone());
    reg.finish();
    (requests, rounds, read, pauses, fake)
}

/// Batch reads in rounds: a warm base in one request (two past 200
/// packages); a cold one (the server fetches some within a request and the
/// rest on after it, ready on the next ask) in two rounds, asked again soon;
/// one whose every package is only fetched on after the answer in two, the
/// first making no progress; rounds the server's busy fetch slots hold up
/// retried, three times at most, then left to single reads; the batch
/// phase's deadline ending the retries early, and the single reads' wait
/// budget never doing so.
#[test]
fn batch_reads_go_in_rounds() {
    super::super::resolution::TEST_ROUND_WAIT.with(|w| w.set(Some(std::time::Duration::from_millis(20))));
    // (requests, rounds, read)
    let warm = |n| { let (q, r, read, _) = prefetch_on(n, |_| ()); (q, r, read) };
    assert_eq!(warm(80), (1, 1, 80));
    assert_eq!(warm(250), (2, 1, 250));
    let cold = |n, budget| {
        let (q, r, read, _) = prefetch_on(n, |f| f.fetch_budget = budget);
        (q, r, read)
    };
    assert_eq!(cold(80, 8), (2, 2, 80));
    assert_eq!(cold(250, 8), (4, 2, 250));
    assert_eq!(cold(80, 0), (2, 2, 80));
    let stalled = |stalls| {
        let (q, r, read, _) = prefetch_on(80, |f| f.stalled = stalls);
        (q, r, read)
    };
    // Two rounds held up, the third reads everything: no single reads.
    assert_eq!(stalled(2), (3, 3, 80));
    // Held up for good: the first round and three more, then single reads.
    assert_eq!(stalled(100), (4, 4, 0));
    // The phase's deadline reached by the first stall's wait: none is made.
    super::super::resolution::TEST_BATCH_DEADLINE.with(|d| d.set(Some(std::time::Duration::from_millis(30))));
    assert_eq!(stalled(100), (1, 1, 0));
    super::super::resolution::TEST_BATCH_DEADLINE.with(|d| d.set(None));
    // The single reads' wait budget is theirs: the batch waits past it.
    super::super::resolution::TEST_WAIT_BUDGET.with(|w| w.set(Some(std::time::Duration::from_millis(30))));
    assert_eq!(stalled(2), (3, 3, 80));
    super::super::resolution::TEST_WAIT_BUDGET.with(|w| w.set(None));
    // A read the server goes on with for three asks: asked again shortly,
    // never taken for a stall, however long it takes.
    let (q, r, read, _) = prefetch_on(80, |f| (f.fetch_budget, f.fetch_asks) = (0, 3));
    assert_eq!((q, r, read), (4, 4, 80));
    super::super::resolution::TEST_ROUND_WAIT.with(|w| w.set(None));
}

/// What follows which round: a short wait after one with reads still going
/// on, twice that after one with none (the server's network taken).
#[test]
fn the_wait_after_a_round_depends_on_what_is_still_being_read() {
    let w = std::time::Duration::from_millis(20);
    super::super::resolution::TEST_ROUND_WAIT.with(|c| c.set(Some(w)));
    let (_, _, read, pauses, _) = prefetch_waits(10, |f| (f.fetch_budget, f.fetch_asks) = (0, 2));
    assert_eq!((read, pauses), (10, vec![w, w]));
    let (_, _, read, pauses, _) = prefetch_waits(10, |f| f.stalled = 2);
    assert_eq!((read, pauses), (10, vec![w * 2, w * 2]));
    super::super::resolution::TEST_ROUND_WAIT.with(|c| c.set(None));
}

/// The batch phase is bounded as a whole, requests and waits together, and
/// in rounds: what is left then is read singly when needed.
#[test]
fn the_batch_phase_is_bounded() {
    super::super::resolution::TEST_ROUND_WAIT.with(|c| c.set(Some(std::time::Duration::from_millis(5))));
    // Never ready: thirty rounds at most (the server's window of twelve
    // requests made short, so its limit costs no time here).
    let never = |f: &mut Fake| (f.fetch_budget, f.fetch_asks, f.window) = (0, 1000, std::time::Duration::from_millis(1));
    let (_, rounds, read, pauses, _) = prefetch_waits(5, never);
    assert_eq!((rounds, read, pauses.len()), (30, 0, 29), "no wait after the last round");
    // A deadline that the rounds' requests and waits reach first.
    super::super::resolution::TEST_BATCH_DEADLINE.with(|d| d.set(Some(std::time::Duration::from_millis(60))));
    let started = std::time::Instant::now();
    let (_, rounds, read, _) = prefetch_on(5, never);
    assert!(rounds < 30 && read == 0, "{rounds} rounds");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    // Requests count: a deadline passed while one was answered sends no other.
    super::super::resolution::TEST_BATCH_DEADLINE.with(|d| d.set(Some(std::time::Duration::from_millis(50))));
    let (requests, _, read, _) = prefetch_on(250, |f| {
        never(f);
        f.batch_delay = std::time::Duration::from_millis(80);
    });
    assert_eq!((requests, read), (1, 0));
    super::super::resolution::TEST_BATCH_DEADLINE.with(|d| d.set(None));
    super::super::resolution::TEST_ROUND_WAIT.with(|c| c.set(None));
}

/// The batch deadline is the command's, not each phase's: a second prefetch
/// after the first used it up has only what is left of it.
#[test]
fn every_prefetch_of_a_command_shares_one_batch_deadline() {
    use super::super::resolution::{TEST_BATCH_DEADLINE, TEST_ROUND_WAIT};
    TEST_ROUND_WAIT.with(|c| c.set(Some(std::time::Duration::from_millis(5))));
    TEST_BATCH_DEADLINE.with(|d| d.set(Some(std::time::Duration::from_millis(150))));
    let mut fake = Fake::new();
    let mut items = Vec::new();
    for i in 0..10 {
        let (id, c) = (format!("et/p{i:03}"), format!("{:040x}", i + 1));
        fake.snapshots.push((id.clone(), c.clone(), version(&c, &c, "https://x/p.git", "P", &["P"], "[]")));
        items.push((crate::deps::PkgId::parse(&id).unwrap(), c));
    }
    (fake.fetch_budget, fake.fetch_asks, fake.window) = (0, 1000, std::time::Duration::from_millis(1));
    let (base, _, _fake) = fake.serve_shared();
    let client = crate::cactarium::Client::at(&base);
    let mut reg = super::super::resolution::Registry::new(&client, []);
    reg.prefetch(&items[..5]).unwrap();
    let first = reg.batch_requests;
    assert!(first > 1, "the first phase asks until the deadline: {first}");
    // The first phase stops short of the deadline (no wait that would reach
    // it): the second has what is left of it, a request's time at most.
    let started = std::time::Instant::now();
    reg.prefetch(&items[5..]).unwrap();
    assert!(reg.batch_requests - first <= 1, "{} requests in the second phase", reg.batch_requests - first);
    assert!(started.elapsed() < std::time::Duration::from_millis(60), "{:?}", started.elapsed());
    reg.finish();
    TEST_BATCH_DEADLINE.with(|d| d.set(None));
    TEST_ROUND_WAIT.with(|c| c.set(None));
}

/// The read limit met by a batch request is waited out within the batch
/// phase's own time: a Retry-After past its deadline is not waited for at
/// all (the request is not sent again, and what it asked is left to single
/// reads), and one within it is waited for whatever is left of the single
/// reads' budget.
#[test]
fn the_batch_waits_for_the_read_limit_on_its_own_time() {
    use super::super::resolution::{TEST_BATCH_DEADLINE, TEST_WAIT_BUDGET};
    let full = |f: &mut Fake| {
        (f.in_window, f.window_start, f.window, f.retry_after) =
            (12, Some(std::time::Instant::now()), std::time::Duration::from_secs(1), true);
    };
    TEST_BATCH_DEADLINE.with(|d| d.set(Some(std::time::Duration::from_millis(500))));
    let started = std::time::Instant::now();
    let (q, r, read, pauses, _) = prefetch_waits(5, full);
    assert_eq!((q, r, read, pauses.len()), (1, 1, 0, 0));
    assert!(started.elapsed() < std::time::Duration::from_millis(500), "{:?}", started.elapsed());
    TEST_BATCH_DEADLINE.with(|d| d.set(None));
    TEST_WAIT_BUDGET.with(|w| w.set(Some(std::time::Duration::from_millis(30))));
    let (q, r, read, pauses, _) = prefetch_waits(5, full);
    assert_eq!((q, r, read, pauses), (2, 1, 5, vec![std::time::Duration::from_secs(1)]));
    TEST_WAIT_BUDGET.with(|w| w.set(None));
}

/// An item a batch answered unusably is not asked in a batch again in the
/// command, by a later prefetch either: what it needs is a single read.
#[test]
fn an_unusable_item_is_never_batched_again() {
    let one = format!("{:040x}", 1);
    let mut fake = Fake::new();
    let mut items = Vec::new();
    for i in 0..5 {
        let (id, c) = (format!("et/p{i:03}"), format!("{:040x}", i + 1));
        fake.snapshots.push((id.clone(), c.clone(), version(&c, &c, "https://x/p.git", "P", &["P"], "[]")));
        items.push((crate::deps::PkgId::parse(&id).unwrap(), c));
    }
    fake.wrong_commit_at.push(one);
    let (base, _, _fake) = fake.serve_shared();
    let client = crate::cactarium::Client::at(&base);
    let mut reg = super::super::resolution::Registry::new(&client, []);
    reg.prefetch(&items).unwrap();
    reg.prefetch(&items).unwrap();
    reg.prefetch(&items[..1]).unwrap();
    assert_eq!(reg.batch_requests, 1);
    assert!(!reg.batch_read(&items[0].0, &items[0].1));
    reg.finish();
}

/// Answers cactup cannot use never fail a command: an unusable item (a
/// version at another commit, another commit echoed) is read singly and not
/// asked again; an unusable answer (one item short, a body that is no JSON)
/// ends the batch phase, everything in it read singly. A package's name
/// echoed in other case is the same package.
#[test]
fn unusable_batch_answers_fall_back_to_single_reads() {
    let one = format!("{:040x}", 1);
    // (requests, rounds, read)
    let (q, r, read, _) = prefetch_on(5, |f| f.wrong_commit_at.push(one.clone()));
    assert_eq!((q, r, read), (1, 1, 4));
    // The same of a version that froze the commit, answered at another.
    let (q, r, read, _) = prefetch_on(5, |f| {
        f.packages.push(("et/p000".into(), vec![version("1.0.0", &one, "https://x/p.git", "P", &["P"], "[]")]));
        f.wrong_commit_at.push(one.clone());
    });
    assert_eq!((q, r, read), (1, 1, 4));
    let (q, r, read, _) = prefetch_on(5, |f| f.echo_other_at.push(one.clone()));
    assert_eq!((q, r, read), (1, 1, 4));
    let (q, r, read, _) = prefetch_on(5, |f| f.shout = true);
    assert_eq!((q, r, read), (1, 1, 5));
    let (q, r, read, _) = prefetch_on(5, |f| f.short_answer = true);
    assert_eq!((q, r, read), (1, 1, 0));
    let (q, r, read, _) = prefetch_on(5, |f| f.garbage_answer = true);
    assert_eq!((q, r, read), (1, 1, 0));
    // Through a whole add: the base read singly where the plan needs it.
    for unusable in 0..3 {
        let (s, _, fake) = unfrozen_base(3, |tmp, mut fake| {
            match unusable {
                0 => fake.short_answer = true,
                1 => fake.garbage_answer = true,
                _ => fake.shout = true,
            }
            let (url, c) = upstream(tmp, "dep", &[("Dep/interface.ccl", "IMPLEMENTS: dep\n")]);
            let claims = format!("[{}]", depends("et/r0", ">=0.0.0", &["U0"]));
            fake.package("max/dep0", vec![version("1.0.0", &c, &url, "Dep", &["Dep"], &claims)])
        });
        let got = records(&s, &["max/dep0"], &AddFlags::default()).unwrap_or_else(|e| panic!("{unusable}: {e:#}"));
        assert_eq!(record_of(&got, "et/r0").2, ["Arr0/U0"]);
        let singles = if unusable == 2 { 0 } else { 1 };
        assert_eq!(fake.lock().unwrap().unfrozen_reads, singles, "{unusable}");
    }
}

/// What the batch keeps for the resolver: a package read untrusted is a
/// snapshot that says so (the plan's untrusted confirmation rests on it);
/// a status this cactup does not know is taken as not read, and asked
/// again; a 429 on the request is waited out, and the request sent again.
#[test]
fn batch_reads_keep_what_the_single_read_would() {
    use crate::deps::Facts;
    let one = format!("{:040x}", 1);
    let mut fake = Fake::new();
    fake.snapshots.push(("et/p".into(), one.clone(), version(&one, &one, "https://x/p.git", "P", &["P"], "[]")));
    fake.untrusted_at.push(one.clone());
    let (base, _, _fake) = fake.serve_shared();
    let client = crate::cactarium::Client::at(&base);
    let mut reg = super::super::resolution::Registry::new(&client, []);
    let id = crate::deps::PkgId::parse("et/p").unwrap();
    reg.prefetch(&[(id.clone(), one.clone())]).unwrap();
    let snap = reg.snapshot(&id, &one).unwrap().expect("read in the batch");
    assert!(snap.untrusted);
    reg.finish();

    super::super::resolution::TEST_ROUND_WAIT.with(|c| c.set(Some(std::time::Duration::from_millis(5))));
    let (q, r, read, _) = prefetch_on(5, |f| f.novel_status = 1);
    assert_eq!((q, r, read), (2, 2, 5));
    super::super::resolution::TEST_ROUND_WAIT.with(|c| c.set(None));

    // The server's window full when the batch asks: Retry-After, then read.
    let (q, r, read, _) = prefetch_on(5, |f| {
        (f.in_window, f.window_start, f.window, f.retry_after) =
            (12, Some(std::time::Instant::now()), std::time::Duration::from_secs(1), true);
    });
    assert_eq!((q, r, read), (2, 1, 5));
}

/// A batch request counts in the server's window of reads made now: a single
/// read that meets the limit after it waits for the window the batch opened,
/// not for a whole window from then.
#[test]
fn a_batch_request_opens_the_read_window() {
    use crate::deps::Facts;
    super::super::resolution::TEST_READ_WINDOW.with(|w| w.set(Some(std::time::Duration::from_secs(1))));
    let (one, two) = (format!("{:040x}", 1), format!("{:040x}", 2));
    let mut fake = Fake::new();
    for c in [&one, &two] {
        fake.snapshots.push(("et/p".into(), c.clone(), version(c, c, "https://x/p.git", "P", &["P"], "[]")));
    }
    let mut fake = fake.package("et/p", vec![version("1.0.0", &"9".repeat(40), "https://x/p.git", "P", &["P"], "[]")]);
    (fake.in_window, fake.window_start, fake.window) = (11, Some(std::time::Instant::now()), std::time::Duration::from_secs(1));
    let (base, _, _fake) = fake.serve_shared();
    let client = crate::cactarium::Client::at(&base);
    let mut reg = super::super::resolution::Registry::new(&client, []);
    let id = crate::deps::PkgId::parse("et/p").unwrap();
    reg.prefetch(&[(id.clone(), one.clone())]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    let started = std::time::Instant::now();
    assert!(reg.snapshot(&id, &two).unwrap().is_some());
    reg.finish();
    assert!(started.elapsed() < std::time::Duration::from_millis(700), "{:?}", started.elapsed());
    super::super::resolution::TEST_READ_WINDOW.with(|w| w.set(None));
}

/// An added package at a commit is read in the batch, like the base, never
/// singly.
#[test]
fn an_added_package_at_a_commit_is_read_in_the_batch() {
    let (s, log, fake) = unfrozen_base(1, |tmp, mut fake| {
        let (url, c) = upstream(tmp, "c", &[("C/interface.ccl", "IMPLEMENTS: c\n")]);
        let (durl, d) = upstream(tmp, "d", &[("D/interface.ccl", "IMPLEMENTS: d\n")]);
        fake.snapshots.push(("max/c".into(), c.clone(), version(&c, &c, &url, "C", &["C"], "[]")));
        fake.package("max/c", vec![version("1.0.0", &"7".repeat(40), &url, "C", &["C"], "[]")])
            .package("max/d", vec![version("1.0.0", &d, &durl, "D", &["D"], "[]")])
    });
    let c = fake.lock().unwrap().snapshots.iter().find(|(p, _, _)| p == "max/c").unwrap().1.clone();
    s.add(&[&format!("max/c#{c}")], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    log.lock().unwrap().clear();
    s.remove(&["max/c"], RemoveFlags { dry_run: true, ..remove_yes() }).unwrap_or_else(|e| panic!("{e:#}"));
    {
        let log = log.lock().unwrap();
        assert!(!log.iter().any(|(p, _)| p.contains(&format!("/versions/{c}"))), "{log:?}");
        assert!(log.iter().any(|(p, b)| p.ends_with("/packages/read") && b.contains("max/c")), "{log:?}");
    }
    // Its commit gone from Cactarium: the batch's own words say so,
    // beside the package that cannot be read.
    s.add(&["max/d"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    fake.lock().unwrap().snapshots.retain(|(p, _, _)| p != "max/c");
    let err = format!("{:#}", s.remove(&["max/d"], remove_yes()).unwrap_err());
    assert!(err.contains(&format!("no commit {c} in max/c's repository (is it pushed?)")), "{err}");
}

/// Rounds held up for good leave the base to single reads, made only for
/// what the plan depends on.
#[test]
fn held_up_batch_reads_fall_back_to_single_reads() {
    super::super::resolution::TEST_ROUND_WAIT.with(|w| w.set(Some(std::time::Duration::from_millis(20))));
    let (s, _, fake) = unfrozen_base(3, |tmp, mut fake| {
        fake.stalled = 100;
        let (url, c) = upstream(tmp, "dep", &[("Dep/interface.ccl", "IMPLEMENTS: dep\n")]);
        let claims = format!("[{}]", depends("et/r0", ">=0.0.0", &["U0"]));
        fake.package("max/dep0", vec![version("1.0.0", &c, &url, "Dep", &["Dep"], &claims)])
    });
    let got = records(&s, &["max/dep0"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(record_of(&got, "et/r0").2, ["Arr0/U0"]);
    let f = fake.lock().unwrap();
    assert_eq!((f.batch_requests, f.unfrozen_reads), (4, 1));
    drop(f);
    super::super::resolution::TEST_ROUND_WAIT.with(|w| w.set(None));
}

/// A Cactarium from before batch reads (its 404 says its API generation):
/// single reads, as before, without a word about it. One whose answer says
/// no API generation is no Cactarium (a wrong address).
#[test]
fn a_cactarium_without_batch_reads_reads_singly() {
    let (requests, rounds, read, fake) = prefetch_on(5, |f| f.batch = false);
    assert_eq!((requests, rounds, read), (1, 1, 0));
    assert_eq!(fake.lock().unwrap().batch_requests, 0);
    let (base, _) = crate::fetch::download::test_server::serve_with_body(1, |_, _| {
        ("404 Not Found", String::new(), b"<html>nope</html>".to_vec())
    });
    let client = crate::cactarium::Client::at(&base);
    let mut reg = super::super::resolution::Registry::new(&client, []);
    let items = [(crate::deps::PkgId::parse("et/p").unwrap(), "a".repeat(40))];
    let err = format!("{:#}", reg.prefetch(&items).unwrap_err());
    reg.finish();
    assert!(err.contains("does not look like a Cactarium"), "{err}");
}

/// A fake whose state a panic poisoned fails the next lock, as
/// `Mutex::lock().unwrap()` does.
#[test]
#[should_panic(expected = "poisoned")]
fn a_poisoned_fake_fails_the_next_lock() {
    let fake = std::sync::Arc::new(FakeLock(Mutex::new(Fake::new()), std::time::Duration::from_millis(50)));
    let held = fake.clone();
    let _ = std::thread::spawn(move || {
        let _guard = held.lock().unwrap();
        panic!("a panic while the fake is locked");
    })
    .join();
    let _ = fake.lock().unwrap().batch_requests;
}

/// A statement that locks the fake twice fails, never hangs.
#[test]
#[should_panic(expected = "locked twice in one statement")]
fn a_fake_locked_twice_fails_rather_than_hangs() {
    let fake = FakeLock(Mutex::new(Fake::new()), std::time::Duration::from_millis(50));
    let _ = (fake.lock().unwrap().batch_requests, fake.lock().unwrap().batch_fetches);
}

/// A base with et/carpet frozen at 1.0.0 (2.0.0 published too) and et/x at a
/// commit no version froze, whose read there claims `x_claims`; its one
/// thorn, MyCarpet, shares no name with et/carpet's. max/new has a thorn of
/// its own; max/app needs et/carpet's Carpet at ^1.0. Without `carpet`, the
/// base has et/x alone.
fn gap_base(x_claims: &str, carpet: bool) -> (Setup, Log, Shared) {
    let tmp = tempfile::tempdir().unwrap();
    let (curl, c1) = upstream(tmp.path(), "carpet", &[("Carpet/interface.ccl", "IMPLEMENTS: carpet\n")]);
    let (xurl, cx) = upstream(tmp.path(), "x", &[("MyCarpet/interface.ccl", "IMPLEMENTS: mycarpet\n")]);
    let (nurl, n) = upstream(tmp.path(), "new", &[("New/interface.ccl", "IMPLEMENTS: new\n")]);
    let (aurl, a) = upstream(tmp.path(), "app", &[("App/interface.ccl", "IMPLEMENTS: app\n")]);
    let mut fake = Fake::new()
        .package("et/carpet", vec![
            version("2.0.0", &"2".repeat(40), &curl, "Carpet", &["Carpet"], "[]"),
            version("1.0.0", &c1, &curl, "Carpet", &["Carpet"], "[]"),
        ])
        .package("et/x", vec![version("1.0.0", &"9".repeat(40), &xurl, "X", &["MyCarpet"], "[]")])
        .package("max/new", vec![version("1.0.0", &n, &nurl, "New", &["New"], "[]")])
        .package("max/app", vec![version("1.0.0", &a, &aurl, "App", &["App"], &format!("[{}]", depends("et/carpet", "^1.0", &["Carpet"])))])
        .pin(&curl, "et/carpet", Some("1.0.0"))
        .pin(&xurl, "et/x", None);
    fake.snapshots.push(("et/x".into(), cx.clone(), version(&cx, &cx, &xurl, "X", &["MyCarpet"], x_claims)));
    let mut base = section(&xurl, &cx, &["X/MyCarpet"]);
    if carpet {
        base = [section(&curl, &c1, &["Carpet/Carpet"]), base].concat();
    }
    let (cactarium, log, fake) = fake.serve_shared();
    (Setup::new(tmp, &base, &cactarium), log, fake)
}

/// Gap (i): a base package at an unfrozen commit says it is incompatible
/// with a package the plan adds. Read in the batch, its claim refuses the
/// plan; a Cactarium without batch reads leaves it unread, and the plan goes
/// through (the gap, as before).
#[test]
fn a_read_stand_in_s_incompatible_with_refuses_what_the_plan_adds() {
    let claims = r#"[{"kind":"incompatible-with","target":"max/new","range":null,"note":null}]"#;
    let (s, _, fake) = gap_base(claims, true);
    let err = format!("{:#}", records(&s, &["max/new"], &AddFlags::default()).unwrap_err());
    assert!(err.contains("says it is incompatible with max/new"), "{err}");
    fake.lock().unwrap().batch = false;
    records(&s, &["max/new"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
}

/// Gap (ii): a base package at an unfrozen commit depends on exactly the
/// release of another base package the plan would move. Read in the batch,
/// its dependency holds the move back.
#[test]
fn a_read_stand_in_s_dependency_holds_back_a_move() {
    let claims = format!("[{}]", depends("et/carpet", "=1.0.0", &["Carpet"]));
    let (s, _, fake) = gap_base(&claims, true);
    let flags = AddFlags { upgrade: true, ..AddFlags::default() };
    let err = format!("{:#}", records(&s, &["et/carpet@=2.0.0"], &flags).unwrap_err());
    assert!(err.contains("et/x#") && err.contains("=1.0.0"), "{err}");
    fake.lock().unwrap().batch = false;
    records(&s, &["et/carpet@=2.0.0"], &flags).unwrap_or_else(|e| panic!("{e:#}"));
}

/// Gap (iii): the base has et/x, at a commit no version froze, a fork of
/// et/carpet whose thorns were renamed (no thorn name in common, so nothing
/// made a read of it necessary). Read in the batch, it is the fork that takes
/// max/app's dependency on et/carpet, as a frozen fork would, and lacks the
/// thorn it names: refused, never et/carpet added beside its fork. A
/// Cactarium without batch reads leaves it unread, and et/carpet comes in
/// beside it (the gap, as before).
#[test]
fn a_read_stand_in_fork_with_renamed_thorns_stands_in() {
    let claims = r#"[{"kind":"fork-of","target":"et/carpet","range":"=1.0.0","note":null}]"#;
    let (s, _, fake) = gap_base(claims, false);
    let err = format!("{:#}", records(&s, &["max/app"], &AddFlags::default()).unwrap_err());
    assert!(err.contains("et/x#") && err.contains("has no Carpet, which max/app@1.0.0 needs"), "{err}");
    fake.lock().unwrap().batch = false;
    let got = records(&s, &["max/app"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    let mut added: Vec<&str> = got.iter().map(|r| r.package.as_str()).collect();
    added.sort();
    assert_eq!(added, ["et/carpet", "max/app"], "{got:?}");
}

/// `remove` reads the base in a batch too: a base package at an unfrozen
/// commit that depends on what is being removed is a dependent, and
/// `--no-deps` the way past. Without batch reads it stays unread, and only
/// the short "not checked" line speaks of it.
#[test]
fn remove_finds_a_base_package_that_depends_on_what_goes() {
    let claims = format!("[{}]", depends("max/new", "^1.0", &["New"]));
    let (s, _, fake) = gap_base(&claims, false);
    fake.lock().unwrap().batch = false;
    s.add(&["max/new"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    fake.lock().unwrap().batch = true;
    let err = format!("{:#}", s.remove(&["max/new"], remove_yes()).unwrap_err());
    assert!(err.contains("Other packages depend on max/new") && err.contains("et/x#"), "{err}");
    assert!(err.contains("--no-deps"), "{err}");
    fake.lock().unwrap().batch = false;
    s.remove(&["max/new"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
}

/// A base whose et/carpet (1.0.0 at c1; 2.0.0 at c2, its branch's tip) is
/// pinned at c1 or follows its branch, beside et/x (at a commit no version
/// froze, read in the batch) and et/f (frozen at 1.0.0), both depending on
/// et/carpet >=1.0.0. `head`: whether Cactarium can read et/carpet's tip
/// (else a commit no version froze, which it has never seen, follows 2.0.0).
/// `tag`: the tag 2.0.0 was frozen from (in the upstream too), which `add`
/// writes as the replacement's `!REPO_BRANCH`.
fn replaced_base(pinned: bool, head: bool, tag: Option<&str>) -> (Setup, Shared) {
    let tmp = tempfile::tempdir().unwrap();
    let (curl, c1) = upstream(tmp.path(), "carpet", &[("Carpet/interface.ccl", "IMPLEMENTS: carpet\n")]);
    let (xurl, cx) = upstream(tmp.path(), "x", &[("X/interface.ccl", "IMPLEMENTS: x\n")]);
    let (furl, cf) = upstream(tmp.path(), "f", &[("F/interface.ccl", "IMPLEMENTS: f\n")]);
    let needs = format!("[{}]", depends("et/carpet", ">=1.0.0", &["Carpet"]));
    let mut fake = Fake::new()
        .package("et/carpet", vec![version("1.0.0", &c1, &curl, "Carpet", &["Carpet"], "[]")])
        .package("et/x", vec![version("1.0.0", &"9".repeat(40), &xurl, "X", &["X"], "[]")])
        .package("et/f", vec![version("1.0.0", &cf, &furl, "F", &["F"], &needs)])
        .pin(&curl, "et/carpet", Some("1.0.0"))
        .pin(&xurl, "et/x", None)
        .pin(&furl, "et/f", Some("1.0.0"));
    fake.snapshots.push(("et/x".into(), cx.clone(), version(&cx, &cx, &xurl, "X", &["X"], &needs)));
    let mut carpet = section(&curl, &c1, &["Carpet/Carpet"]);
    if !pinned {
        carpet = carpet.replace(&format!("!REPO_COMMIT = {c1}\n"), "");
    }
    let base = [carpet, section(&xurl, &cx, &["X/X"]), section(&furl, &cf, &["F/F"])].concat();
    let (cactarium, _, fake) = fake.serve_shared();
    let setup = Setup::new(tmp, &base, &cactarium);
    // Then et/carpet 2.0.0, its branch's new tip.
    let up = setup.inst.root.parent().unwrap().join("up/carpet");
    let c2 = crate::fetch::tests::commit_files(&up, &[("Carpet/interface.ccl", "IMPLEMENTS: carpet\n"), ("README", "2\n")]);
    if let Some(tag) = tag {
        let repo = gix::open(&up).unwrap();
        repo.reference(format!("refs/tags/{tag}"), c2, gix::refs::transaction::PreviousValue::Any, "tag").unwrap();
    }
    let c2 = c2.to_string();
    {
        let mut f = fake.lock().unwrap();
        let mut two = version("2.0.0", &c2, &curl, "Carpet", &["Carpet"], "[]");
        if let Some(tag) = tag {
            two = two.replace("\"tag\":null", &format!("\"tag\":\"{tag}\""));
        }
        f.packages.iter_mut().find(|(p, _)| p == "et/carpet").unwrap().1.insert(0, two);
    }
    if !head {
        crate::fetch::tests::commit_files(&up, &[("Carpet/interface.ccl", "IMPLEMENTS: carpet\n"), ("README", "3\n")]);
    }
    (setup, fake)
}

/// Removing a package that replaced a base repo brings that base repo back,
/// at the base's pin (or its branch's tip): what depends on the package and
/// is met by what comes back is no dependent, frozen or read in the batch.
/// What comes back that cannot be read is said beside a refusal.
#[test]
fn removing_a_replacement_counts_the_base_that_comes_back() {
    // Pinned; following its branch, the replacement frozen from a plain tag,
    // or from a track's (with a slash): the replacement's `!REPO_BRANCH`,
    // which the remove must not take for the base's branch.
    for (pinned, head, tag) in [(true, false, None), (false, true, Some("v2.0.0")), (false, true, Some("mixed-precision/v2.0.0"))] {
        let (s, _) = replaced_base(pinned, head, tag);
        let upgrade = AddFlags { upgrade: true, yes: true, ..AddFlags::default() };
        s.add(&["et/carpet@=2.0.0"], upgrade).unwrap_or_else(|e| panic!("pinned {pinned}: {e:#}"));
        assert!(s.meta().added.iter().any(|a| a.package == "et/carpet" && a.replaces == ["carpet"]), "pinned {pinned}");
        if let Some(tag) = tag {
            // As on a real installation: the fetch record names the tag.
            let state = crate::fetch::FetchState::read(&s.inst.root).unwrap().unwrap();
            assert_eq!(state.repos["carpet"].branch.as_deref(), Some(tag));
            // Any fetch of the base list (an `inst refetch` too) puts the
            // clone back on the remote's default branch, never on the tag.
            let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
            let list = thornlist::parse(&pristine).unwrap();
            let mut progress = prodash::tree::Root::new().add_child("plan");
            let plan = crate::fetch::plan(&list, &s.inst.root, &mut progress).unwrap();
            let carpet = plan.git.iter().find(|g| g.repo == "carpet").expect("carpet fetched, not skipped");
            let (branch, _) = crate::fetch::git::head_of(&s.inst.root.parent().unwrap().join("up/carpet")).unwrap();
            assert_eq!(carpet.branch.as_deref(), Some(branch.as_str()), "tag {tag}");
        }
        // The replacement left the base's clone detached at its own pin:
        // cactup's doing, so the remove restores it (onto the branch, for a
        // base that follows one).
        s.remove(&["et/carpet"], remove_yes()).unwrap_or_else(|e| panic!("pinned {pinned} tag {tag:?}: {e:#}"));
        let clone = s.inst.root.join("Cactus/repos/carpet");
        let (branch, head) = crate::fetch::git::head_of(&clone).unwrap();
        if !pinned {
            let tip = crate::fetch::git::head_of(&s.inst.root.parent().unwrap().join("up/carpet")).unwrap();
            assert_eq!((branch, head), tip, "back on the base's branch, at its tip (tag {tag:?})");
        }
    }
    // The user moved HEAD after the add (detached at another commit): theirs,
    // left alone, and the remove refuses before anything is asked.
    let (s, _) = replaced_base(false, true, None);
    s.add(&["et/carpet@=2.0.0"], AddFlags { upgrade: true, yes: true, ..AddFlags::default() }).unwrap();
    let clone = s.inst.root.join("Cactus/repos/carpet");
    let mine = crate::fetch::tests::commit_files(&clone, &[("Carpet/interface.ccl", "IMPLEMENTS: carpet\n"), ("README", "2\n"), ("MINE", "x\n")]);
    assert_eq!(crate::fetch::git::head_of(&clone).unwrap(), (crate::fetch::git::DETACHED.to_owned(), mine));
    let flags = RemoveFlags { dry_run: true, ..remove_yes() };
    let err = format!("{:#}", s.remove(&["et/carpet"], flags).unwrap_err());
    assert!(err.contains("Repo carpet has detached HEAD") && err.contains("is still added"), "{err}");
    // A base that follows its branch, whose tip Cactarium cannot read:
    // what comes back is unknown, and the refusal says so.
    let (s, _) = replaced_base(false, false, None);
    s.add(&["et/carpet@=2.0.0"], AddFlags { upgrade: true, yes: true, ..AddFlags::default() }).unwrap();
    let start = Start::read(&s.ctx, false, Doing::Removing).unwrap();
    let base = thornlist::parse(&start.pristine).unwrap();
    let target = [crate::deps::PkgId::parse("et/carpet").unwrap()];
    let err = format!("{:#}", super::plan_remove_with_deps(&s.ctx, &start, &base, &target).unwrap_err());
    assert!(err.contains("Other packages depend on et/carpet"), "{err}");
    assert!(err.contains("cannot be read") && err.contains("base repo carpet: et/carpet at "), "{err}");
}

/// What comes back with a removal holds a dependent only when it meets the
/// dependency: max/d needs et/carpet ^2.0 (the replacement's release), or
/// a thorn only the replacement enabled. The base's 1.0.0 that comes back
/// meets neither: max/d stays a dependent, and the remove refuses.
#[test]
fn what_comes_back_holds_only_what_it_meets() {
    for (range, needs) in [("^2.0.0", "Carpet"), (">=1.0.0", "CarpetX")] {
        let (s, fake) = replaced_base(true, true, None);
        let up = s.inst.root.parent().unwrap().join("up");
        crate::fetch::git::testrepo::init(&up.join("d"));
        let d = crate::fetch::tests::commit_files(&up.join("d"), &[("D/interface.ccl", "IMPLEMENTS: d\n")]).to_string();
        let durl = up.join("d").display().to_string();
        let curl = up.join("carpet").display().to_string();
        {
            let mut f = fake.lock().unwrap();
            let claims = format!("[{}]", depends("et/carpet", range, &[needs]));
            f.packages.push(("max/d".into(), vec![version("1.0.0", &d, &durl, "D", &["D"], &claims)]));
            // 2.0.0 has CarpetX too, which the base does not check out.
            let carpet = &mut f.packages.iter_mut().find(|(p, _)| p == "et/carpet").unwrap().1;
            let c2 = carpet[0].split("\"commit\":\"").nth(1).unwrap()[..40].to_owned();
            carpet[0] = version("2.0.0", &c2, &curl, "Carpet", &["Carpet", "CarpetX"], "[]");
        }
        let up_c = up.join("carpet");
        crate::fetch::tests::commit_files(&up_c, &[("Carpet/interface.ccl", "IMPLEMENTS: carpet\n"), ("CarpetX/interface.ccl", "IMPLEMENTS: carpetx\n")]);
        let flags = AddFlags { upgrade: true, yes: true, ..AddFlags::default() };
        let _ = s.add(&["max/d"], flags);
        if !s.meta().added.iter().any(|a| a.package == "et/carpet" && !a.replaces.is_empty()) {
            panic!("{range} {needs}: et/carpet was not upgraded in the base's place: {:?}", s.meta().added);
        }
        let err = format!("{:#}", s.remove(&["et/carpet"], remove_yes()).unwrap_err());
        assert!(err.contains("Other packages depend on et/carpet") && err.contains("max/d@1.0.0"), "{range} {needs}: {err}");
    }
}

/// What comes back that cannot be read might need what the removal sweeps
/// away: every orphan is shown with that, so the confirmation covers it,
/// whether the base that cannot be read comes back with the target or with
/// an orphan. (No dependent of et/carpet here: et/f and et/x claim nothing.)
#[test]
fn an_unread_returning_base_is_said_beside_every_orphan() {
    let (s, fake) = replaced_base(false, false, None);
    let up = s.inst.root.parent().unwrap().join("up");
    crate::fetch::git::testrepo::init(&up.join("p"));
    let p = crate::fetch::tests::commit_files(&up.join("p"), &[("P/interface.ccl", "IMPLEMENTS: p\n")]).to_string();
    let purl = up.join("p").display().to_string();
    {
        let mut f = fake.lock().unwrap();
        f.packages.push(("max/p".into(), vec![version("1.0.0", &p, &purl, "P", &["P"], "[]")]));
        let ef = &mut f.packages.iter_mut().find(|(p, _)| p == "et/f").unwrap().1[0];
        let needs = depends("et/carpet", ">=1.0.0", &["Carpet"]);
        assert!(ef.contains(&needs));
        *ef = ef.replace(&format!("[{needs}]"), "[]");
        for (_, _, x) in f.snapshots.iter_mut().filter(|(p, _, _)| p == "et/x") {
            *x = x.replace(&format!("[{needs}]"), "[]");
        }
        let carpet = &mut f.packages.iter_mut().find(|(p, _)| p == "et/carpet").unwrap().1[0];
        assert!(carpet.contains("\"claims\":[]"));
        *carpet = carpet.replace("\"claims\":[]", &format!("\"claims\":[{}]", depends("max/p", ">=1.0.0", &["P"])));
    }
    s.add(&["et/carpet@=2.0.0"], AddFlags { upgrade: true, yes: true, ..AddFlags::default() }).unwrap();
    assert!(s.meta().added.iter().any(|a| a.package == "max/p"), "{:?}", s.meta().added);
    let start = Start::read(&s.ctx, false, Doing::Removing).unwrap();
    let base = thornlist::parse(&start.pristine).unwrap();
    let target = [crate::deps::PkgId::parse("et/carpet").unwrap()];
    let plan = super::plan_remove_with_deps(&s.ctx, &start, &base, &target).unwrap();
    assert_eq!(plan.orphans.len(), 1, "{plan:?}");
    let why = plan.orphans[0].why();
    assert!(why.contains("unless a returning base repo needs it"), "{why}");
    assert!(plan.render("remove").contains("unless a returning base repo needs it"));
}

/// Where a removal reads the base that comes back is where its restore puts
/// it: the one decision both make. Here the remote renamed its default
/// branch after the base was cloned (a new `trunk`, at a commit
/// unreadable by Cactarium); the clone still knows its own (`master`), and
/// the restore goes there, so the removal reads the base there too.
#[test]
fn the_base_comes_back_where_the_restore_puts_it() {
    let (s, _fake) = replaced_base(false, true, None);
    s.add(&["et/carpet@=2.0.0"], AddFlags { upgrade: true, yes: true, ..AddFlags::default() }).unwrap();
    let up = s.inst.root.parent().unwrap().join("up/carpet");
    let (branch, tip) = crate::fetch::git::head_of(&up).unwrap();
    let repo = gix::open(&up).unwrap();
    repo.reference("refs/heads/trunk", tip, gix::refs::transaction::PreviousValue::Any, "branch").unwrap();
    std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/trunk\n").unwrap();
    let c3 = crate::fetch::tests::commit_files(&up, &[("Carpet/interface.ccl", "IMPLEMENTS: carpet\n"), ("README", "3\n")]);
    assert_eq!(crate::fetch::git::head_of(&up).unwrap(), ("trunk".to_owned(), c3));
    s.remove(&["et/carpet"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let clone = s.inst.root.join("Cactus/repos/carpet");
    assert_eq!(crate::fetch::git::head_of(&clone).unwrap(), (branch, tip), "back on the clone's own default");
}

/// `add` and `remove` look at the clones they touch only: a base clone the
/// list stops pinning (cactup's checkout, detached at the old pin, whose
/// branch only its remote can tell) is neither probed nor asked about by
/// either; a whole-list fetch would ask.
#[test]
fn add_and_remove_ask_nothing_of_clones_they_do_not_touch() {
    let (s, _fake) = replaced_base(true, true, None);
    let pristine_path = s.inst.source_thornlist();
    let pristine = std::fs::read_to_string(&pristine_path).unwrap();
    let list = thornlist::parse(&pristine).unwrap();
    let x = list.components().iter().find(|c| c.repo == "x").unwrap();
    let (xurl, xpin) = (x.url.clone().unwrap(), x.commit.clone().unwrap());
    let unpinned = pristine.replacen(&format!("!REPO_COMMIT = {xpin}\n"), "", 1);
    assert_ne!(unpinned, pristine, "x's pin is in the list as expected");
    std::fs::write(&pristine_path, &unpinned).unwrap();
    let asked_x = || crate::fetch::git::ASKED.lock().unwrap().iter().any(|u| u == &xurl);
    s.add(&["et/carpet@=2.0.0"], AddFlags { upgrade: true, yes: true, ..AddFlags::default() }).unwrap();
    s.remove(&["et/carpet"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(!asked_x(), "x was asked about");
    // A fetch of the whole list asks.
    let mut progress = prodash::tree::Root::new().add_child("plan");
    crate::fetch::plan(&thornlist::parse(&unpinned).unwrap(), &s.inst.root, &mut progress).unwrap();
    assert!(asked_x(), "the whole list's fetch would have asked about x");
}

/// A clone being restored that no branch can be found for (no `!REPO_BRANCH`,
/// a clone made pinned that knows no remote branch, no label, a remote that
/// does not answer) is refused before anyone is asked, never left to fail
/// the fetch after the confirmation.
#[test]
fn a_restore_with_no_branch_is_refused_before_the_prompt() {
    let (s, fake) = replaced_base(false, true, None);
    // Nothing depends on et/carpet: the refusal can only be the restore's.
    {
        let mut f = fake.lock().unwrap();
        let needs = depends("et/carpet", ">=1.0.0", &["Carpet"]);
        let ef = &mut f.packages.iter_mut().find(|(p, _)| p == "et/f").unwrap().1[0];
        *ef = ef.replace(&format!("[{needs}]"), "[]");
        for (_, _, x) in f.snapshots.iter_mut().filter(|(p, _, _)| p == "et/x") {
            *x = x.replace(&format!("[{needs}]"), "[]");
        }
    }
    s.add(&["et/carpet@=2.0.0"], AddFlags { upgrade: true, yes: true, ..AddFlags::default() }).unwrap();
    let clone = s.inst.root.join("Cactus/repos/carpet");
    // The clone knows no remote branch, its record no label, and its remote is gone.
    let git_dir = gix::open(&clone).unwrap().git_dir().to_owned();
    let _ = std::fs::remove_dir_all(git_dir.join("refs/remotes"));
    let _ = std::fs::remove_file(git_dir.join("packed-refs"));
    assert_eq!(crate::fetch::git::default_branch(&clone), None);
    let state_path = s.inst.root.join(".cactup/fetch-state.toml");
    let state = std::fs::read_to_string(&state_path).unwrap();
    let state: String = state.lines().filter(|l| !l.starts_with("branch = ")).map(|l| format!("{l}\n")).collect();
    std::fs::write(&state_path, state).unwrap();
    std::fs::remove_dir_all(s.inst.root.parent().unwrap().join("up/carpet")).unwrap();
    let flags = RemoveFlags { dry_run: true, ..remove_yes() };
    let err = format!("{:#}", s.remove(&["et/carpet"], flags).unwrap_err());
    assert!(err.contains("Repo carpet has no branch to go back to") && err.contains("is still added"), "{err}");
    // A refetch of the list leaves such a clone where it is, skipped as detached, never planned
    // onto no branch to fail after.
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let mut progress = prodash::tree::Root::new().add_child("plan");
    let plan = crate::fetch::plan(&thornlist::parse(&pristine).unwrap(), &s.inst.root, &mut progress).unwrap();
    assert!(plan.git.iter().all(|g| g.repo != "carpet"), "{:?}", plan.git);
    let skipped = plan.skipped.iter().find(|s| s.repo == "carpet").expect("carpet skipped");
    assert!(matches!(skipped.reason, crate::fetch::git::DirtyReason::DetachedHead(_)), "{:?}", skipped.reason);
}

/// Only what a removal can bring back is asked for: the bases of the
/// targets and of what may be swept with them, never of a package the
/// user named that stays. And an interrupt is never a reading, even with
/// nothing to ask (the flag is the process's: run in a child).
#[test]
fn only_what_the_removal_brings_back_is_read() {
    let (s, _fake) = replaced_base(true, true, None);
    s.add(&["et/carpet@=2.0.0"], AddFlags { upgrade: true, yes: true, ..AddFlags::default() }).unwrap();
    let start = Start::read(&s.ctx, false, Doing::Removing).unwrap();
    let base = thornlist::parse(&start.pristine).unwrap();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    let mut reg = super::super::resolution::Registry::new(&client, ["et/carpet"]);
    let carpet = crate::deps::PkgId::parse("et/carpet").unwrap();
    let other = [crate::deps::PkgId::parse("et/x").unwrap()].into_iter().collect();
    let read = super::super::resolution::returning_base(&mut reg, &s.inst.root, &base, &start.meta.added, &other).unwrap();
    assert!(read.0.is_empty() && read.1.is_empty(), "nothing of et/carpet's is read: {:?}", read.1);
    let bringing = [carpet.clone()].into_iter().collect();
    let (back, unread) = super::super::resolution::returning_base(&mut reg, &s.inst.root, &base, &start.meta.added, &bringing).unwrap();
    assert_eq!((back.len(), unread.len()), (1, 0), "{unread:?}");
    assert_eq!(back[0].0, carpet);
    if std::env::var_os("CACTUP_TEST_INTERRUPT_CHILD").is_some() {
        gix::interrupt::IS_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
        let e = super::super::resolution::returning_base(&mut reg, &s.inst.root, &base, &start.meta.added, &other).unwrap_err();
        assert!(format!("{e:#}").contains("Interrupted"), "{e:#}");
        return;
    }
    reg.finish();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["commands::add::deps_tests::only_what_the_removal_brings_back_is_read", "--exact", "--test-threads=1"])
        .env("CACTUP_TEST_INTERRUPT_CHILD", "1")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && said.contains("1 passed"), "{said}{}", String::from_utf8_lossy(&out.stderr));
}

/// Taking away the thorns cactup gave a base package (an extension) gives
/// back the base as it was: a base package depending on that package with
/// no thorns named lived with that, and is no dependent of the extension.
#[test]
fn removing_an_extension_is_not_held_by_a_base_package_s_whole_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    let (zurl, cz) = upstream(tmp.path(), "z", &[("A/interface.ccl", "IMPLEMENTS: a\n"), ("B/interface.ccl", "IMPLEMENTS: b\n")]);
    let (yurl, cy) = upstream(tmp.path(), "y", &[("Y/interface.ccl", "IMPLEMENTS: y\n")]);
    let whole = format!("[{}]", depends("et/z", ">=0.0.0", &[]));
    let fake = Fake::new()
        .package("et/z", vec![version("1.0.0", &cz, &zurl, "Z", &["A", "B"], "[]")])
        .package("et/y", vec![version("1.0.0", &cy, &yurl, "Y", &["Y"], &whole)])
        .pin(&zurl, "et/z", Some("1.0.0"))
        .pin(&yurl, "et/y", Some("1.0.0"));
    let base = [section(&zurl, &cz, &["Z/A"]), section(&yurl, &cy, &["Y/Y"])].concat();
    let (cactarium, _, _fake) = fake.serve_shared();
    let s = Setup::new(tmp, &base, &cactarium);
    s.add(&["et/z/B"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    s.remove(&["et/z"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.meta().added.is_empty());
}

/// A base package at an unfrozen commit the resolver read (a dependency
/// needs a thorn of it) is no longer one whose dependencies are unknown: the
/// "cannot check" count leaves it out, as `remove` does.
#[test]
fn a_stand_in_the_resolver_read_is_checked() {
    // A Cactarium without batch reads: the resolver reads what it needs.
    let (s, _, _) = unfrozen_base(3, |tmp, mut fake| {
        fake.batch = false;
        let (url, c) = upstream(tmp, "dep", &[("Dep/interface.ccl", "IMPLEMENTS: dep\n")]);
        let claims = format!("[{}]", depends("et/r0", ">=0.0.0", &["U0"]));
        fake.package("max/dep0", vec![version("1.0.0", &c, &url, "Dep", &["Dep"], &claims)])
    });
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let list = thornlist::parse(&pristine).unwrap();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    let mut reg = super::super::resolution::Registry::new(&client, []);
    let mut set = super::super::resolution::present(&mut reg, &s.inst.root, &list, &[], None, &Default::default()).unwrap();
    assert_eq!(set.unchecked.len(), 3);
    let specs = [crate::deps::NamedSpec::parse("max/dep0").unwrap()];
    let rules = crate::deps::Rules::default();
    let request = crate::deps::Request { specs: &specs, present: &set.present, rules: &rules, options: Default::default() };
    crate::deps::resolve(&request, &mut reg).unwrap_or_else(|e| panic!("{e:#}"));
    set.forget_read(&reg);
    reg.finish();
    assert_eq!(set.unchecked.iter().map(|u| u.split('#').next().unwrap()).collect::<Vec<_>>(), ["et/r1", "et/r2"]);
}

/// A base clone the user detached at an unfrozen commit: more thorns of its
/// package are only links into it (never fetched or pinned), so the add
/// goes ahead. A repo the add does fetch, left alone by the fetch (local
/// changes in its clone), refuses before anyone is asked: under `--dry-run`
/// too.
#[test]
fn a_detached_base_clone_takes_more_thorns_and_refusals_come_before_the_prompt() {
    // The base follows its branch, now at a2 (2.0.0); the user detaches
    // HEAD there.
    let it = admbase_full("^2.0", false, true, |f| f);
    let s = &it.setup;
    let clone = s.inst.root.join("Cactus/repos/admbase");
    std::fs::write(clone.join(".git/HEAD"), format!("{}\n", it.a2)).unwrap();
    assert_eq!(super::clone_head(&s.inst.root, "Cactus", "admbase").as_deref(), Some(it.a2.as_str()));
    s.add(&["max/app"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let added = s.added();
    assert!(added.contains(&("et/admbase".to_owned(), vec!["ADMBase/StaticConformal".to_owned()])), "{added:?}");
    assert!(s.inst.root.join("Cactus/arrangements/ADMBase/StaticConformal/interface.ccl").is_file());

    s.add(&["max/solo"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let solo = s.inst.root.join("Cactus/repos/solo");
    std::fs::write(solo.join("Solo/interface.ccl"), "IMPLEMENTS: solo\n# edited\n").unwrap();
    let before = std::fs::read_to_string(s.inst.live_thornlist().unwrap()).unwrap();
    let flags = AddFlags { dry_run: true, ..AddFlags::default() };
    let err = format!("{:#}", s.add(&["max/solo", "max/app3"], flags).unwrap_err());
    assert!(err.contains("Repo solo in the installation has local modifications"), "{err}");
    assert_eq!(std::fs::read_to_string(s.inst.live_thornlist().unwrap()).unwrap(), before);
    let _ = (&it.a1, &it.log, &it.fake);
}

/// Twenty base repos at unfrozen commits, each with a thorn `T{i}` enabled
/// and `U{i}` not, each read at its commit by the fake; `extra` adds
/// packages to the fake.
fn unfrozen_base(n: usize, extra: impl FnOnce(&std::path::Path, Fake) -> Fake) -> (Setup, Log, Shared) {
    let tmp = tempfile::tempdir().unwrap();
    let mut base = String::new();
    let mut fake = Fake::new();
    for i in 0..n {
        let name = format!("r{i}");
        let (url, c) = upstream(
            tmp.path(),
            &name,
            &[
                (&format!("T{i}/interface.ccl"), "IMPLEMENTS: x\n"),
                (&format!("U{i}/interface.ccl"), "IMPLEMENTS: u\nUSES INCLUDE HEADER: u.h\n"),
                (&format!("U{i}/src/u.c"), "#include \"u.h\"\n"),
            ],
        );
        base.push_str(&section(&url, &c, &[&format!("Arr{i}/T{i}")]));
        let id = format!("et/{name}");
        let thorns = [format!("T{i}"), format!("U{i}")];
        let thorns: Vec<&str> = thorns.iter().map(String::as_str).collect();
        fake = fake.package(&id, vec![version("1.0.0", &format!("{:040x}", i + 1), &url, &format!("Arr{i}"), &thorns, "[]")]);
        fake.snapshots.push((id.clone(), c.clone(), version(&c, &c, &url, &format!("Arr{i}"), &thorns, "[]")));
        fake = fake.pin(&url, &id, None);
    }
    let fake = extra(tmp.path(), fake);
    let (cactarium, log, fake) = fake.serve_shared();
    (Setup::new(tmp, &base, &cactarium), log, fake)
}

/// The requests at a commit the fake got: (all of them, those it answered
/// 429).
fn commit_reads(log: &Log, fake: &Shared) -> (usize, usize) {
    let all = log.lock().unwrap().iter().filter(|(p, _)| p.contains("/versions/")).count();
    (all, all - fake.lock().unwrap().unfrozen_reads)
}

/// A plan needing fifteen reads at unfrozen commits, past the server's
/// twelve a window: it waits once for the window (an older server: no
/// Retry-After), then reads the rest, and goes through. With Retry-After it
/// waits for that.
#[test]
fn fifteen_reads_wait_once_for_the_read_limit() {
    for retry_after in [false, true] {
        // With Retry-After, the server's window (1 s) is what it says, and
        // cactup's own fallback (10 s) must not be what it waits for.
        let window = if retry_after { std::time::Duration::from_secs(1) } else { std::time::Duration::from_secs(2) };
        let fallback = if retry_after { std::time::Duration::from_secs(10) } else { window };
        super::super::resolution::TEST_READ_WINDOW.with(|w| w.set(Some(fallback)));
        // Single reads (a Cactarium without batch reads).
        let (s, log, fake) = unfrozen_base(15, |tmp, mut fake| {
            fake.batch = false;
            let (url, c) = upstream(tmp, "dep", &[("Dep/interface.ccl", "IMPLEMENTS: dep\n")]);
            let claims: Vec<String> = (0..15).map(|i| depends(&format!("et/r{i}"), ">=0.0.0", &[&format!("U{i}")])).collect();
            fake.package("max/dep15", vec![version("1.0.0", &c, &url, "Dep", &["Dep"], &format!("[{}]", claims.join(",")))])
        });
        {
            let mut f = fake.lock().unwrap();
            (f.window, f.retry_after) = (window, retry_after);
        }
        log.lock().unwrap().clear();
        let started = std::time::Instant::now();
        let got = records(&s, &["max/dep15"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(got.iter().filter(|r| r.package.starts_with("et/r")).count(), 15);
        // Fifteen read, the thirteenth asked twice: once refused, once after the wait.
        assert_eq!(commit_reads(&log, &fake), (16, 1), "retry-after {retry_after}");
        assert!(started.elapsed() >= window.mul_f32(0.5), "it waited");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "for Retry-After, not the fallback");
    }
    super::super::resolution::TEST_READ_WINDOW.with(|w| w.set(None));
}

/// A server that keeps answering 429: a command waits at most its budget
/// in all, then leaves the reads still to make unread, at once.
#[test]
fn waiting_for_the_read_limit_stops_at_the_budget() {
    use super::super::resolution::{TEST_READ_WINDOW, TEST_WAIT_BUDGET};
    TEST_READ_WINDOW.with(|w| w.set(Some(std::time::Duration::from_secs(1))));
    TEST_WAIT_BUDGET.with(|w| w.set(Some(std::time::Duration::from_millis(1200))));
    // Single reads (a Cactarium without batch reads).
    let (s, log, fake) = unfrozen_base(3, |tmp, mut fake| {
        fake.batch = false;
        let (url, c) = upstream(tmp, "dep", &[("Dep/interface.ccl", "IMPLEMENTS: dep\n")]);
        let claims: Vec<String> = (0..3).map(|i| depends(&format!("et/r{i}"), ">=0.0.0", &[&format!("U{i}")])).collect();
        fake.package("max/dep3", vec![version("1.0.0", &c, &url, "Dep", &["Dep"], &format!("[{}]", claims.join(",")))])
    });
    fake.lock().unwrap().always_limited = true;
    log.lock().unwrap().clear();
    let started = std::time::Instant::now();
    let err = format!("{:#}", records(&s, &["max/dep3"], &AddFlags::default()).unwrap_err());
    // Without the budget: three reads, two waits of a second each.
    assert!(started.elapsed() < std::time::Duration::from_secs(4), "{:?}", started.elapsed());
    assert!(err.contains("its read limit held after waiting"), "{err}");
    // The first read asked three times (two waits, the second cut short by
    // the budget); the other two once each, without waiting.
    assert_eq!(commit_reads(&log, &fake), (5, 5));
    TEST_READ_WINDOW.with(|w| w.set(None));
    TEST_WAIT_BUDGET.with(|w| w.set(None));
}

/// A package whose thorns clash by name with fourteen base packages at
/// unfrozen commits: each is read once (any could be a fork standing in),
/// and the claims of its thorns nothing enables read nothing more.
#[test]
fn a_package_clashing_with_fourteen_base_packages_reads_each_once() {
    super::super::resolution::TEST_READ_WINDOW.with(|w| w.set(Some(std::time::Duration::from_secs(2))));
    // Single reads (a Cactarium without batch reads).
    let (s, log, fake) = unfrozen_base(14, |tmp, mut fake| {
        fake.batch = false;
        let (url, c) = upstream(tmp, "wide", &[("W/interface.ccl", "IMPLEMENTS: w\n")]);
        let thorns: Vec<String> = (0..14).map(|i| format!("T{i}")).collect();
        let thorns: Vec<&str> = thorns.iter().map(String::as_str).collect();
        fake.package("max/wide", vec![version("1.0.0", &c, &url, "Wide", &thorns, "[]")])
    });
    fake.lock().unwrap().window = std::time::Duration::from_secs(2);
    log.lock().unwrap().clear();
    let err = format!("{:#}", records(&s, &["max/wide"], &AddFlags::default()).unwrap_err());
    assert!(err.contains("both have a thorn"), "{err}");
    assert_eq!(fake.lock().unwrap().unfrozen_reads, 14);
    assert_eq!(commit_reads(&log, &fake), (15, 1));
    super::super::resolution::TEST_READ_WINDOW.with(|w| w.set(None));
}

/// A base repo whose package an `[[added]]` entry also is (and does not
/// only extend) becomes a repo of no package: no stand-in, never read.
#[test]
fn a_base_repo_renamed_to_a_pseudo_package_is_no_stand_in() {
    let (s, log, fake) = unfrozen_base(1, |_, f| f);
    let added = AddedPackage {
        package: "et/r0".into(),
        version: "1.0.0".into(),
        tag: None,
        commit: format!("{:040x}", 1),
        url: "https://elsewhere/r0.git".into(),
        repo: "r0-elsewhere".into(),
        thorns: vec![AddedThorn { checkout: "Arr0/U0".into(), repo_path: "$2".into() }],
        replaces: Vec::new(),
        contact: None,
        libraries: Vec::new(),
        flesh: Vec::new(),
    };
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let list = thornlist::parse(&pristine).unwrap();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    let mut reg = super::super::resolution::Registry::new(&client, []);
    let set = super::super::resolution::present(&mut reg, &s.inst.root, &list, &[added], None, &Default::default());
    reg.finish();
    let set = set.unwrap_or_else(|e| panic!("{e:#}"));
    let pseudo = set.present.iter().find(|p| p.release.id.as_str() == "local:r0").expect("the base repo as a pseudo-package");
    assert!(!pseudo.stand_in);
    assert_eq!(fake.lock().unwrap().unfrozen_reads, 0);
    // Not asked for in a batch either.
    let log = log.lock().unwrap();
    assert!(!log.iter().any(|(p, b)| p.ends_with("/packages/read") && b.contains("et/r0")), "{log:?}");
}

/// A base repo at an unfrozen commit that the lookup says is a registered
/// flesh is one, however its tree is laid out; the tree decides only where
/// the lookup does not say so.
#[test]
fn the_lookup_says_which_unfrozen_base_repo_is_the_flesh() {
    for registered in [false, true] {
        // A stand-in: a Cactarium without batch reads.
        let (s, _, fake) = unfrozen_base(1, |_, mut f| {
            f.batch = false;
            f
        });
        if registered {
            let url = fake.lock().unwrap().pins[0].0.clone();
            fake.lock().unwrap().flesh.push(url);
        }
        let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
        let list = thornlist::parse(&pristine).unwrap();
        let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
        let mut reg = super::super::resolution::Registry::new(&client, []);
        let set = super::super::resolution::present(&mut reg, &s.inst.root, &list, &[], None, &Default::default());
        reg.finish();
        let set = set.unwrap_or_else(|e| panic!("{e:#}"));
        let r0 = set.present.iter().find(|p| p.release.id.as_str() == "et/r0").expect("et/r0");
        assert!(r0.stand_in);
        assert_eq!(r0.release.flesh, registered);
        // Its thorns are its clone's: the one checked out and the one not.
        let names: Vec<&str> = r0.release.thorns.iter().map(|t| t.thorn.as_str()).collect();
        assert_eq!(names, ["T0", "U0"], "registered {registered}");
        // Read from the CCL alone: the thorn's sources (which include a header) never.
        assert!(r0.release.thorns[1].facts.needs_includes.is_empty());
    }
}

/// Why a base package at an unfrozen commit was not read, said as it is,
/// for each status the server answers a read at a commit with: not there
/// (404: is it pushed?), not readable there (400: the server's own words),
/// not served (403), the git host down (502). Each leaves that one package
/// unread and the search goes on: the refusal is the search's, naming what
/// needed which thorn of it, and the way past that finds a plan.
#[test]
fn an_unread_base_package_says_why() {
    type Answer = fn(&mut Fake, &str);
    let cases: [(&str, Answer, String); 6] = [
        ("404", |f, _| f.snapshots.clear(), "et/admbase is at a commit unknown to Cactarium (is it pushed?)".into()),
        ("400", |f, a1| f.bad_at.push(a1.to_owned()), "invalid manifest: no [package] table".into()),
        ("400 too large", |f, a1| f.too_large_at.push(a1.to_owned()), "its tree is too large to read".into()),
        ("403", |f, a1| f.refuse_at.push(a1.to_owned()), "it serves an untrusted package only at approved releases (one awaiting review needs --risky-allow-untrusted-artifacts and an admin's token)".into()),
        ("502", |f, a1| f.down_at.push(a1.to_owned()), "it could not reach the package's repository just now".into()),
        ("504 from a proxy", |f, a1| f.proxy_timeout_at.push(a1.to_owned()), "it (or a proxy in front of it) failed (504); try again later".into()),
    ];
    // Read in a batch, or singly (a Cactarium without batch reads): the same words.
    for ((status, set, why), batch) in cases.iter().flat_map(|c| [(c, true), (c, false)]) {
        let it = admbase_unfrozen("^1.0");
        set(&mut it.fake.lock().unwrap(), &it.a1);
        it.fake.lock().unwrap().batch = batch;
        let err = format!("{:#}", it.setup.add(&["max/app"], yes()).unwrap_err());
        let needs = format!("max/app@1.0.0 needs StaticConformal of et/admbase#{}", &it.a1[..12]);
        assert!(err.contains(&needs) && err.contains("Cactarium did not read et/admbase there: "), "{status} batch {batch}: {err}");
        assert!(err.contains(why.as_str()), "{status} batch {batch}: {err}");
        if *status == "400" {
            assert!(err.contains(&format!("et/admbase at commit {}: ", &it.a1[..12])), "{status}: {err}");
        }
        assert!(err.contains("Pass --upgrade to move it to a release"), "{status}: {err}");
        // In a batch, the item's status is the answer: no single read made
        // (but when a proxy failed the batch request itself).
        let singles = it.log.lock().unwrap().iter().filter(|(p, _)| p.contains("/versions/")).count();
        if batch && !status.contains("proxy") {
            assert_eq!(singles, 0, "{status}");
        }
        let _ = &it.a2;
    }
}

/// `add` brought in max/lib for a thorn it gave a base package at an
/// unfrozen commit (that thorn's claim needs it): `remove max/lib` reads the
/// base package there and refuses, naming it.
#[test]
fn remove_reads_an_unfrozen_base_package_cactup_gave_thorns() {
    let it = admbase_unfrozen("^1.0");
    let s = &it.setup;
    let libdir = tempfile::tempdir().unwrap();
    let (lurl, l) = upstream(libdir.path(), "lib", &[("Lib/interface.ccl", "IMPLEMENTS: lib\n")]);
    {
        let mut f = it.fake.lock().unwrap();
        f.packages.push(("max/lib".into(), vec![version("1.0.0", &l, &lurl, "Lib", &["Lib"], "[]")]));
        let claims = format!(
            r#"[{{"kind":"dependency","target":"max/lib","range":"^1.0","needs":["Lib"],"needed-by":["StaticConformal"],"note":null}}]"#
        );
        let (_, _, json) = &mut f.snapshots[0];
        let read: serde_json::Value = serde_json::from_str(json).unwrap();
        let mut read = read;
        read["claims"] = serde_json::from_str(&claims).unwrap();
        *json = read.to_string();
    }
    s.add(&["max/app"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.added().iter().any(|(p, _)| p == "max/lib"), "{:?}", s.added());
    let err = format!("{:#}", s.remove(&["max/lib"], remove_yes()).unwrap_err());
    assert!(err.contains("Other packages depend on max/lib") && err.contains("et/admbase"), "{err}");
    let _ = (&it.a2, &it.log);
}

/// A base whose flesh is et/cactus 1.0.0 at `f` (`repos/flesh`; its
/// `Makefile`, `lib` and `src` linked into the root; 2.0.0 is at `f2`, a
/// commit later), beside base repo tb (et/tb, thorn Base/T). max/mp and
/// max/mx 1.0.0 are fleshes of their own (max/mf too, from a repository
/// named like the base's flesh clone)
/// (cloned as `repos/cactus-mp`, `repos/cactus-mx`). max/z4c 1.0.0 (thorn
/// Z4c) needs the flesh max/mp at >=1.0, max/fu (FU) at ^1.0; max/fe (FE)
/// needs et/cactus ^1.0.
struct Flesh {
    setup: Setup,
    f: String,
    f2: String,
    mp: String,
    fake: Shared,
}

fn flesh() -> Flesh {
    let tmp = tempfile::tempdir().unwrap();
    let tree = |who: &str| -> Vec<(String, String)> {
        ["Makefile", "lib/make.pl", "src/main.c"].iter().map(|f| (f.to_string(), format!("{who}\n"))).collect()
    };
    let up = |name: &str, who: &str| {
        let files = tree(who);
        let files: Vec<(&str, &str)> = files.iter().map(|(f, c)| (f.as_str(), c.as_str())).collect();
        upstream(tmp.path(), name, &files)
    };
    let (furl, f) = up("cactus", "base");
    let (_, f2) = up("cactus", "base2");
    let (mpurl, mp) = up("cactus-mp", "mp");
    let (mxurl, mx) = up("cactus-mx", "mx");
    let (mfurl, mf) = up("flesh", "mf");
    let (fuurl, fu) = upstream(tmp.path(), "fu", &[("FU/interface.ccl", "IMPLEMENTS: fu\n")]);
    let (feurl, fe) = upstream(tmp.path(), "fe", &[("FE/interface.ccl", "IMPLEMENTS: fe\n")]);
    let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\n")]);
    let (tburl, tb) = upstream(tmp.path(), "tb", &[("T/interface.ccl", "IMPLEMENTS: t\n")]);
    let claims = format!("[{}]", depends("max/mp", ">=1.0", &[]));
    let mut fake = Fake::new()
        .package(
            "et/cactus",
            vec![version_with("2.0.0", &f2, &furl, &[], "[]", true), version_with("1.0.0", &f, &furl, &[], "[]", true)],
        )
        .package("max/mp", vec![version_with("1.0.0", &mp, &mpurl, &[], "[]", true)])
        .package("max/mx", vec![version_with("1.0.0", &mx, &mxurl, &[], "[]", true)])
        .package("max/mf", vec![version_with("1.0.0", &mf, &mfurl, &[], "[]", true)])
        .package("max/fu", vec![version("1.0.0", &fu, &fuurl, "FU", &["FU"], &format!("[{}]", depends("max/mp", "^1.0", &[])))])
        .package("max/fe", vec![version("1.0.0", &fe, &feurl, "FE", &["FE"], &format!("[{}]", depends("et/cactus", "^1.0", &[])))])
        .package("max/z4c", vec![version("1.0.0", &z, &zurl, "Z4c", &["Z4c"], &claims)])
        .package("et/tb", vec![version("1.0.0", &tb, &tburl, "Base", &["T"], "[]")])
        .pin(&furl, "et/cactus", Some("1.0.0"))
        .pin(&tburl, "et/tb", Some("1.0.0"));
    fake.flesh.push(furl.clone());
    let (cactarium, _, fake) = fake.serve_shared();
    let base = format!(
        "\n!TARGET = $ROOT\n!TYPE = git\n!URL = {furl}\n!NAME = flesh\n!REPO_COMMIT = {f}\n!CHECKOUT = Makefile lib src\n{}",
        section(&tburl, &tb, &["Base/T"])
    );
    Flesh { setup: Setup::new(tmp, &base, &cactarium), f, f2, mp, fake }
}

impl Flesh {
    fn cactus(&self) -> std::path::PathBuf {
        self.setup.inst.root.join("Cactus")
    }

    /// Every link at the root points into `repos/<clone>`, and the flesh
    /// read through them is `who`'s.
    fn flesh_is(&self, clone: &str, who: &str) {
        let cactus = self.cactus();
        for item in ["Makefile", "lib", "src"] {
            let to = std::fs::read_link(cactus.join(item)).unwrap_or_else(|e| panic!("{item}: {e}"));
            assert_eq!(to, std::path::Path::new("repos").join(clone).join(item), "{item}");
        }
        assert_eq!(std::fs::read_to_string(cactus.join("Makefile")).unwrap(), format!("{who}\n"));
        assert_eq!(std::fs::read_to_string(cactus.join("src/main.c")).unwrap(), format!("{who}\n"));
        assert!(cactus.join("arrangements/Base/T/interface.ccl").is_file(), "the base's thorns stay");
    }

    fn live(&self) -> String {
        std::fs::read_to_string(self.setup.inst.live_thornlist().unwrap()).unwrap()
    }

    fn pristine(&self) -> String {
        std::fs::read_to_string(self.setup.inst.source_thornlist()).unwrap()
    }

    fn schema(&self) -> String {
        let path = self.setup.inst.cactup_dir().join("installation.toml");
        std::fs::read_to_string(path).unwrap().lines().next().unwrap().to_owned()
    }
}

/// `cactup add max/mp`, a flesh: it takes the place of the base's flesh
/// (keyed on the flesh, whatever its name or URL), the root's links point
/// into its clone, the plan says every config needs a rebuild, and the
/// record is written as v4. `remove` points the links back into the base's
/// flesh clone, fetched again when it was pruned.
#[test]
fn a_flesh_package_takes_the_base_flesh_s_place_and_gives_it_back() {
    let it = flesh();
    let s = &it.setup;
    it.flesh_is("flesh", "base");
    let pristine = it.pristine();
    let base = thornlist::parse(&pristine).unwrap();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    let (plan, _, _) = plan_add(&client, &s.inst.root, &base, &s.meta(), None, &["max/mp".to_owned()], &yes()).unwrap();
    let text = plan.render("cactup add max/mp");
    assert!(text.contains("base repo flesh with max/mp") && text.contains(crate::deps::FLESH_REBUILD), "{text}");
    assert!(plan.rebuild);

    s.add(&["max/mp"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    it.flesh_is("cactus-mp", "mp");
    let meta = s.meta();
    let mp = &meta.added[0];
    assert_eq!((mp.package.as_str(), mp.repo.as_str(), mp.commit.as_str()), ("max/mp", "cactus-mp", it.mp.as_str()));
    let items: Vec<String> = ["Makefile", "lib", "src"].map(String::from).into();
    assert_eq!((mp.flesh.clone(), mp.replaces.clone()), (items, vec!["flesh".to_owned()]));
    assert_eq!(it.schema(), "schema = 4");
    // The composed list: the base's flesh gone, the package's at the root.
    let live = it.live();
    let list = thornlist::parse(&live).unwrap();
    assert!(list.components().iter().all(|c| c.repo != "flesh"), "{live}");
    let root: Vec<(&str, &str, Option<&str>)> = (list.components().iter())
        .filter(|c| c.target == "Cactus")
        .map(|c| (c.checkout.as_str(), c.repo.as_str(), c.commit.as_deref()))
        .collect();
    let at = Some(it.mp.as_str());
    assert_eq!(root, [("Makefile", "cactus-mp", at), ("lib", "cactus-mp", at), ("src", "cactus-mp", at)], "{live}");
    assert!(build_hint(Some("the new flesh"), false).contains("Rebuild every config for the new flesh"));

    s.remove(&["max/mp"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    it.flesh_is("flesh", "base");
    assert_eq!(it.live(), pristine);
    assert!(s.meta().added.is_empty());
    assert!(s.inst.root.join("Cactus/repos/cactus-mp").is_dir(), "its clone stays, for a prune");

    // Pruned while the package was there: fetched again at the base's pin.
    s.add(&["max/mp"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    std::fs::remove_dir_all(it.cactus().join("repos/flesh")).unwrap();
    s.remove(&["max/mp"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    it.flesh_is("flesh", "base");
    let (_, head) = crate::fetch::git::head_of(&it.cactus().join("repos/flesh")).unwrap();
    assert_eq!(head.to_string(), it.f);
    assert_eq!(it.live(), pristine);
}

/// The base's flesh is et/cactus: moving it (`@=2.0.0 --upgrade`) makes a
/// flesh package in the base's own clone, at the new commit, its links
/// unmoved. A flesh package later takes its place; et/cactus added again
/// then is no base package any more (the base's flesh is gone) but a flesh
/// of its own, in place of that one. Removed, the clone goes back to the
/// base's pin.
#[test]
fn the_base_flesh_s_own_package_moves_in_place() {
    let it = flesh();
    let s = &it.setup;
    let head = || crate::fetch::git::head_of(&it.cactus().join("repos/flesh")).unwrap().1.to_string();
    let upgrade = AddFlags { upgrade: true, yes: true, ..AddFlags::default() };
    s.add(&["et/cactus@=2.0.0"], upgrade).unwrap_or_else(|e| panic!("{e:#}"));
    let meta = s.meta();
    let cactus = &meta.added[0];
    let got = (cactus.package.as_str(), cactus.repo.as_str(), cactus.commit.as_str());
    assert_eq!(got, ("et/cactus", "flesh", it.f2.as_str()));
    assert_eq!((cactus.flesh.len(), cactus.replaces.clone()), (3, vec!["flesh".to_owned()]));
    it.flesh_is("flesh", "base2");
    assert_eq!(head(), it.f2);

    s.add(&["max/mp"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(s.added(), [("max/mp".to_owned(), vec![])]);
    it.flesh_is("cactus-mp", "mp");
    s.add(&["et/cactus"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(s.added(), [("et/cactus".to_owned(), vec![])]);
    it.flesh_is("flesh", "base2");

    s.remove(&["et/cactus"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    it.flesh_is("flesh", "base");
    assert_eq!(head(), it.f);
    assert_eq!(it.live(), it.pristine());
}

/// A flesh needed by another package comes in as a dependency, in place of
/// the base's flesh; removing the dependent takes it along and gives the
/// base's flesh back.
#[test]
fn a_flesh_package_comes_in_as_a_dependency() {
    let it = flesh();
    let s = &it.setup;
    s.add(&["max/z4c"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(s.added(), [("max/z4c".to_owned(), vec!["Z4c/Z4c".to_owned()]), ("max/mp".to_owned(), vec![])]);
    let meta = s.meta();
    assert!(meta.added[1].is_flesh());
    assert!(!meta.added_for["max/mp"].requested);
    it.flesh_is("cactus-mp", "mp");
    assert!(it.cactus().join("arrangements/Z4c/Z4c/interface.ccl").is_file());

    s.remove(&["max/z4c"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.meta().added.is_empty());
    it.flesh_is("flesh", "base");
    assert_eq!(it.live(), it.pristine());
}

/// An installation has one flesh: two flesh packages at once are refused,
/// and a second flesh package later takes the first one's place, which
/// goes from the record.
#[test]
fn one_flesh_package_at_a_time() {
    let it = flesh();
    let s = &it.setup;
    let err = format!("{:#}", s.add(&["max/mp", "max/mx"], yes()).unwrap_err());
    assert!(err.contains("The plan needs two fleshes"), "{err}");
    assert!(s.meta().added.is_empty());
    it.flesh_is("flesh", "base");

    s.add(&["max/mp"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    s.add(&["max/mx"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let meta = s.meta();
    assert_eq!(meta.added.iter().map(|a| a.package.as_str()).collect::<Vec<_>>(), ["max/mx"]);
    assert_eq!(meta.added_for.keys().collect::<Vec<_>>(), ["max/mx"]);
    assert_eq!(meta.added[0].replaces, ["flesh"]);
    it.flesh_is("cactus-mx", "mx");

    s.remove(&["max/mx"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    it.flesh_is("flesh", "base");
    assert_eq!(it.live(), it.pristine());
}

/// A symbolic copy (`inst cp -s`) gets a flesh package and loses it again:
/// its own links move, its source's never do, and nothing is written
/// through a copy link into the source.
#[test]
fn a_symbolic_copy_gets_a_flesh_package() {
    let it = flesh();
    let s = &it.setup;
    let home = s._tmp.path().join("copy");
    let copy = super::super::copy::symbolic_copy(&s.inst, &home, "c").unwrap();
    let path = copy.root.display().to_string();
    s.ctx
        .db
        .update(move |d| {
            let mut entry = d.installations["t"].clone();
            entry.alias = "c".into();
            entry.path = path;
            entry.copied_from = Some("t".into());
            d.installations.insert("c".into(), entry);
            d.active_installation = Some("c".into());
            Ok(())
        })
        .unwrap();
    let source_live = it.live();
    let cactus = copy.root.join("Cactus");
    let source_makefile = it.cactus().join("repos/flesh/Makefile");
    assert_eq!(std::fs::read_link(cactus.join("repos/flesh/Makefile")).unwrap(), source_makefile, "a copy link");

    s.add(&["max/mp"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    for item in ["Makefile", "lib", "src"] {
        assert_eq!(std::fs::read_link(cactus.join(item)).unwrap(), std::path::Path::new("repos/cactus-mp").join(item));
    }
    assert_eq!(std::fs::read_to_string(cactus.join("Makefile")).unwrap(), "mp\n");
    assert!(copy.meta().unwrap().added[0].is_flesh());
    // The source: as it was.
    it.flesh_is("flesh", "base");
    assert_eq!(it.live(), source_live);
    assert!(s.inst.meta().unwrap().added.is_empty());
    assert!(!it.cactus().join("repos/cactus-mp").exists());

    s.remove(&["max/mp"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    for item in ["Makefile", "lib", "src"] {
        assert_eq!(std::fs::read_link(cactus.join(item)).unwrap(), std::path::Path::new("repos/flesh").join(item));
    }
    assert_eq!(std::fs::read_to_string(cactus.join("Makefile")).unwrap(), "base\n");
    assert_eq!(std::fs::read_link(cactus.join("repos/flesh/Makefile")).unwrap(), source_makefile, "still a copy link");
    assert_eq!(std::fs::read_to_string(&source_makefile).unwrap(), "base\n");
    assert!(copy.meta().unwrap().added.is_empty());
}

/// Ctrl-C part way through the links (after `Makefile` and `lib` moved,
/// before `src`): the command fails, the record and the list are as they
/// were, and the root's links all point where they did. For `add`, and for
/// `remove` (the flag is the process's: each case runs in a child).
#[test]
fn an_interrupt_among_the_flesh_links_leaves_a_consistent_tree() {
    fn interrupt_after_lib(c: &Component) {
        if c.checkout == "lib" {
            gix::interrupt::IS_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let Some(case) = std::env::var_os("CACTUP_TEST_INTERRUPT_CHILD") else {
        for case in ["add", "remove"] {
            let name = "commands::add::deps_tests::an_interrupt_among_the_flesh_links_leaves_a_consistent_tree";
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([name, "--exact", "--test-threads=1"])
                .env("CACTUP_TEST_INTERRUPT_CHILD", case)
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&out.stdout);
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success() && said.contains("1 passed"), "{case}: {said}{err}");
        }
        return;
    };
    let it = flesh();
    let s = &it.setup;
    let (clone, who) = match case.to_str() {
        Some("add") => ("flesh", "base"),
        _ => {
            s.add(&["max/mp"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
            ("cactus-mp", "mp")
        }
    };
    let (live, meta) = (it.live(), s.meta());
    crate::fetch::BEFORE_LINK.with(|hook| hook.set(Some(interrupt_after_lib)));
    let err = match case.to_str() {
        Some("add") => s.add(&["max/mp"], yes()),
        _ => s.remove(&["max/mp"], remove_yes()),
    };
    crate::fetch::BEFORE_LINK.with(|hook| hook.set(None));
    let err = format!("{:#}", err.unwrap_err());
    assert!(err.contains("Interrupted"), "{err}");
    it.flesh_is(clone, who);
    assert_eq!(it.live(), live);
    assert_eq!(s.meta().added, meta.added);
}

/// `remove` gives back the flesh of the base as it is now: after a refetch
/// to a base whose flesh is named otherwise (`core`, never fetched), the
/// links point into that clone, fetched for it.
#[test]
fn remove_gives_back_the_flesh_of_the_base_as_it_is_now() {
    let it = flesh();
    let s = &it.setup;
    s.add(&["max/mp"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    // What a refetch to such a base writes: the new base, and the list composed onto it.
    let pristine = it.pristine().replace("!NAME = flesh", "!NAME = core");
    std::fs::write(s.inst.source_thornlist(), &pristine).unwrap();
    std::fs::write(s.inst.live_thornlist().unwrap(), compose(&pristine, &s.meta().added).unwrap()).unwrap();
    s.remove(&["max/mp"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    it.flesh_is("core", "base");
    assert_eq!(it.live(), pristine);
}

/// A flesh coming in never takes the place of a flesh that a package staying
/// needs (an added flesh package, or the base's own): refused, naming the
/// dependents, with `--no-deps` as the way past.
#[test]
fn a_flesh_with_dependents_is_not_displaced() {
    let it = flesh();
    let s = &it.setup;
    s.add(&["max/fu"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let meta = s.meta();
    let err = format!("{:#}", s.add(&["max/mx"], yes()).unwrap_err());
    assert!(err.contains("max/mx@1.0.0 would take the place of the flesh max/mp@1.0.0"), "{err}");
    assert!(err.contains("installed max/fu@1.0.0 (max/mp ^1.0)") && err.contains("pass --no-deps"), "{err}");
    assert_eq!(s.meta().added, meta.added);
    it.flesh_is("cactus-mp", "mp");
    s.add(&["max/mx"], AddFlags { no_deps: true, ..yes() }).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(s.added(), [("max/fu".to_owned(), vec!["FU/FU".to_owned()]), ("max/mx".to_owned(), vec![])]);
    it.flesh_is("cactus-mx", "mx");

    // The base's flesh, which max/fe needs.
    let it = flesh();
    let s = &it.setup;
    s.add(&["max/fe"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let err = format!("{:#}", s.add(&["max/mx"], yes()).unwrap_err());
    assert!(err.contains("the flesh et/cactus@1.0.0") && err.contains("installed max/fe@1.0.0"), "{err}");
    it.flesh_is("flesh", "base");
}

/// A flesh package from another repository whose clone name is the base
/// flesh's is refused, saying so.
#[test]
fn a_flesh_package_named_like_the_base_flesh_clone_is_refused() {
    let it = flesh();
    let s = &it.setup;
    let err = format!("{:#}", s.add(&["max/mf"], yes()).unwrap_err());
    assert!(err.contains("max/mf@1.0.0 and the base's flesh have the same clone name, flesh."), "{err}");
    assert!(err.contains("A flesh package needs a clone name of its own."), "{err}");
    assert!(s.meta().added.is_empty());
    it.flesh_is("flesh", "base");
}

/// An added flesh package moved to a release that is no flesh is refused.
#[test]
fn a_flesh_package_never_moves_to_a_release_that_is_not_a_flesh() {
    let it = flesh();
    let s = &it.setup;
    s.add(&["max/mp"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let up = s._tmp.path().join("up/cactus-mp");
    let mp2 = crate::fetch::tests::commit_files(&up, &[("README", "2\n")]).to_string();
    {
        let mut f = it.fake.lock().unwrap();
        let two = version_with("2.0.0", &mp2, &up.display().to_string(), &[], "[]", false);
        f.packages.iter_mut().find(|(p, _)| p == "max/mp").unwrap().1.insert(0, two);
    }
    let err = format!("{:#}", s.add(&["max/mp@=2.0.0"], yes()).unwrap_err());
    assert!(
        err.starts_with("max/mp@2.0.0 is not a flesh.\nmax/mp is the installation's flesh.\nRemove it first with"),
        "{err}"
    );
    assert_eq!(s.meta().added[0].commit, it.mp);
}

/// After a refetch to a base with no flesh, removing the flesh package takes
/// the root's links into its clone away: the root is the base's again.
#[test]
fn remove_leaves_no_flesh_links_on_a_base_without_a_flesh() {
    let it = flesh();
    let s = &it.setup;
    s.add(&["max/mp"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let pristine = it.pristine();
    let start = pristine.find("\n!TARGET = $ROOT").unwrap();
    let end = start + pristine[start..].find("Makefile lib src\n").unwrap() + "Makefile lib src\n".len();
    let pristine = format!("{}{}", &pristine[..start], &pristine[end..]);
    std::fs::write(s.inst.source_thornlist(), &pristine).unwrap();
    std::fs::write(s.inst.live_thornlist().unwrap(), compose(&pristine, &s.meta().added).unwrap()).unwrap();
    s.remove(&["max/mp"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    for item in ["Makefile", "lib", "src"] {
        assert!(std::fs::symlink_metadata(it.cactus().join(item)).is_err(), "{item}");
    }
    assert!(it.cactus().join("repos/cactus-mp/Makefile").is_file(), "the clone keeps its files");
    assert!(it.cactus().join("arrangements/Base/T/interface.ccl").is_file());
    assert_eq!(it.live(), pristine);
}

/// A config built from a thornlist of its own (`--thornlist`), which keeps
/// naming the base's flesh: the flesh used for its build is where the root's
/// Makefile links, so a flesh package in place of the base's flesh, and the
/// base's back, are a changed flesh to `build`, `config delta` and the
/// submit warning alike (all read the sources this way).
#[test]
fn a_config_s_own_thornlist_sees_the_flesh_swapped() {
    let it = flesh();
    let s = &it.setup;
    let own = it.pristine();
    let cactus = it.cactus();
    let probe = || crate::fetch::probe_sources(&s.inst.root, &cactus, &own).unwrap().unwrap();
    let built = probe();
    assert_eq!(built.flesh.as_deref(), Some("flesh"));
    s.add(&["max/mp"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let now = probe();
    assert_eq!(now.flesh.as_deref(), Some("cactus-mp"));
    // The list's own flesh, unused by the build, is not recorded.
    assert!(!now.heads.contains_key("flesh"), "{:?}", now.heads);
    let (delta, change) = crate::build::source_delta(Some(&built.heads), Some(&now));
    assert_eq!((delta, change.swapped.as_deref()), (crate::build::SourceDelta::Flesh, Some("cactus-mp")));

    // Back: the record of the build with the package (what `now` read) lacks
    // the base's flesh, so this is a swap too.
    s.remove(&["max/mp"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let back = probe();
    let (delta, change) = crate::build::source_delta(Some(&now.heads), Some(&back));
    assert_eq!((delta, change.swapped.as_deref()), (crate::build::SourceDelta::Flesh, Some("flesh")));
    // And a build after that one is up to date.
    assert_eq!(crate::build::source_delta(Some(&back.heads), Some(&probe())).0, crate::build::SourceDelta::Unchanged);
}

// ---- `cactup claims`: overrides kept in the installation, applied by add, remove and check ----


/// What `cactup claims …` does on the setup's installation, as the command line reads it.
fn claims(s: &Setup, words: &str) -> Res<()> {
    claims_args(s, &words.split_whitespace().collect::<Vec<_>>())
}

/// [`claims`], the words already split as a shell would (a key with spaces is one word).
fn claims_args(s: &Setup, words: &[&str]) -> Res<()> {
    use clap::Parser;
    let args = crate::args::Args::try_parse_from(std::iter::once("cactup").chain(words.iter().copied()))
        .unwrap_or_else(|e| panic!("`cactup {}` does not parse: {e}", words.join(" ")));
    match args.command {
        crate::args::Commands::Claims(cmd) => crate::commands::claims::dispatch(&s.ctx, cmd),
        _ => panic!("`cactup {}` is no claims command", words.join(" ")),
    }
}

/// The command a refusal names (`cactup claims override …` in backticks), run as the user would.
fn run_the_hint(s: &Setup, refusal: &str) {
    let start = refusal.find("`cactup claims override").unwrap_or_else(|| panic!("no hint in: {refusal}")) + 1;
    let words = &refusal[start..][..refusal[start..].find('`').unwrap()];
    claims(s, words.strip_prefix("cactup ").unwrap()).unwrap_or_else(|e| panic!("`{words}` failed: {e:#}"));
}

fn overrides(s: &Setup) -> Vec<String> {
    s.meta().claim_overrides.keys().cloned().collect()
}

/// A Cactarium with max/z4c (needs et/carpetx's CarpetX), et/carpetx (CarpetX, Loop, Arith),
/// max/app (incompatible with max/solo), max/solo and max/plain. With `base`, the installation's
/// base has a repo `basecx` (no registered package) with CarpetX and Loop thorns.
fn overridable(base: bool) -> Setup {
    overridable_with_fake(base).0
}

/// [`overridable`], and the fake Cactarium serving it.
fn overridable_with_fake(base: bool) -> (Setup, Shared) {
    let tmp = tempfile::tempdir().unwrap();
    let one = |name: &str, thorns: &[&str]| {
        let files: Vec<(String, String)> =
            thorns.iter().map(|t| (format!("{t}/interface.ccl"), format!("IMPLEMENTS: {}\n", t.to_lowercase()))).collect();
        let files: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        upstream(tmp.path(), name, &files)
    };
    let (zurl, z) = one("z4c", &["Z4c"]);
    let (curl, c) = one("carpetx", &["CarpetX", "Loop", "Arith"]);
    let (aurl, a) = one("app", &["App"]);
    let (surl, so) = one("solo", &["Solo"]);
    let (purl, p) = one("plain", &["Plain"]);
    let (burl, b) = one("basecx", &["CarpetX", "Loop"]);
    let (turl, t2) = one("two", &["Two"]);
    let (_, t1) = upstream(tmp.path(), "two", &[("Two/interface.ccl", "IMPLEMENTS: two\n"), ("README", "1\n")]);
    let hates = r#"{"kind":"incompatible-with","target":"max/solo","range":null,"needs":[],"note":null}"#;
    let (cactarium, _, fake) = Fake::new()
        .package(
            "max/two",
            vec![
                version("2.0.0", &t2, &turl, "Two", &["Two"], &format!("[{}]", depends("et/carpetx", "^1.0", &["Arith"]))),
                version("1.0.0", &t1, &turl, "Two", &["Two"], "[]"),
            ],
        )
        .package("max/z4c", vec![version("1.0.0", &z, &zurl, "Z4c", &["Z4c"], &format!("[{}]", depends("et/carpetx", "^1.0", &["CarpetX"])))])
        .package("et/carpetx", vec![version("1.0.0", &c, &curl, "CarpetX", &["CarpetX", "Loop", "Arith"], "[]")])
        .package("max/app", vec![version("1.0.0", &a, &aurl, "App", &["App"], &format!("[{hates}]"))])
        .package("max/solo", vec![version("1.0.0", &so, &surl, "Solo", &["Solo"], "[]")])
        .package("max/plain", vec![version("1.0.0", &p, &purl, "Plain", &["Plain"], "[]")])
        .serve_shared();
    let text = match base {
        true => section(&burl, &b, &["CarpetX/CarpetX", "CarpetX/Loop"]),
        false => String::new(),
    };
    (Setup::new(tmp, &text, &cactarium), fake)
}

/// `satisfies` turns the refusal that names it into a plan, and removing the override brings the
/// refusal back: the unregistered base repo stands in for the dependency's package.
#[test]
fn a_satisfies_override_lets_a_base_repo_stand_in_for_a_dependency() {
    let s = overridable(true);
    let err = format!("{:#}", s.add(&["max/z4c"], yes()).unwrap_err());
    assert!(err.contains("both have a thorn CarpetX"), "{err}");
    assert!(err.contains("or `cactup claims override satisfies local:basecx et/carpetx`"), "{err}");
    assert!(overrides(&s).is_empty() && s.added().is_empty());

    run_the_hint(&s, &err);
    assert_eq!(overrides(&s), ["satisfies local:basecx et/carpetx"]);
    s.add(&["max/z4c"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(s.added(), [("max/z4c".to_owned(), vec!["Z4c/Z4c".to_owned()])]);

    // Back to the refusal, whichever way the key is spelled.
    s.remove(&["max/z4c"], remove_yes()).unwrap();
    claims_args(&s, &["claims", "override", "--remove", "drop"]).unwrap_err();
    assert_eq!(overrides(&s).len(), 1);
    claims_args(&s, &["claims", "override", "--remove", "satisfies  local:basecx   et/carpetx"]).unwrap();
    assert!(overrides(&s).is_empty());
    let err = format!("{:#}", s.add(&["max/z4c"], yes()).unwrap_err());
    assert!(err.contains("both have a thorn CarpetX"), "{err}");
    // The file stays at the version that holds overrides.
    let text = std::fs::read_to_string(s.inst.cactup_dir().join("installation.toml")).unwrap();
    assert!(text.starts_with("schema = 5"), "{text}");
}

/// `drop` lets an incompatibility through, the refusal names the command that does it, and
/// `check` agrees with the plan both ways: no problem while the claim is dropped, one when it is not.
#[test]
fn a_drop_override_passes_an_incompatible_with_and_check_agrees() {
    let s = overridable(false);
    s.add(&["max/solo"], yes()).unwrap();
    let err = format!("{:#}", s.add(&["max/app"], yes()).unwrap_err());
    assert!(err.contains("`cactup claims override drop max/app:incompatible-with:max/solo` (for good)"), "{err}");
    assert!(err.contains("--ignore-claim max/app:incompatible-with:max/solo (this command)"), "{err}");
    run_the_hint(&s, &err);
    s.add(&["max/app"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    claims(&s, "claims check").unwrap();

    claims_args(&s, &["claims", "override", "--remove", "drop max/app:incompatible-with:max/solo"]).unwrap();
    assert!(overrides(&s).is_empty());
    let meta = s.meta();
    let found = crate::commands::claims::problems(&s.ctx, &s.inst, &meta).unwrap();
    assert_eq!(
        found.lines,
        ["max/app@1.0.0 is incompatible with max/solo@1.0.0. Run `cactup claims override drop max/app:incompatible-with:max/solo`."]
    );
    let err = format!("{:#}", claims(&s, "claims check").unwrap_err());
    assert_eq!(err, "1 problem.");
    // The way past the check names works.
    claims(&s, "claims override drop max/app:incompatible-with:max/solo --note hand-built").unwrap();
    assert_eq!(s.meta().claim_overrides["drop max/app:incompatible-with:max/solo"].note.as_deref(), Some("hand-built"));
    claims(&s, "claims check").unwrap();
    claims(&s, "claims list").unwrap();
    // Set again with another note, it takes it; with none, it keeps the one it has.
    claims(&s, "claims override drop max/app:incompatible-with:max/solo --note other").unwrap();
    assert_eq!(s.meta().claim_overrides["drop max/app:incompatible-with:max/solo"].note.as_deref(), Some("other"));
    claims(&s, "claims override drop max/app:incompatible-with:max/solo").unwrap();
    assert_eq!(s.meta().claim_overrides["drop max/app:incompatible-with:max/solo"].note.as_deref(), Some("other"));
    let err = format!("{:#}", claims_args(&s, &["claims", "override", "drop", "max/app:incompatible-with:max/solo", "--note", "a\nb"]).unwrap_err());
    assert_eq!(err, "A note is one line.");
}

/// `needs` adds a thorn to a dependency's needs: `add` brings it in; `add` of a dependency the
/// manifest lacks holds the package against `remove`, and `drop` of a dependency frees it.
#[test]
fn needs_add_and_drop_overrides_reach_add_and_remove() {
    let s = overridable(false);
    claims(&s, "claims override needs max/z4c:dependency:et/carpetx Loop").unwrap();
    assert_eq!(overrides(&s), ["needs max/z4c:dependency:et/carpetx Loop"]);
    s.add(&["max/z4c"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(
        s.added(),
        [
            ("max/z4c".to_owned(), vec!["Z4c/Z4c".to_owned()]),
            ("et/carpetx".to_owned(), vec!["CarpetX/CarpetX".to_owned(), "CarpetX/Loop".to_owned()]),
        ]
    );
    // The dependency holds et/carpetx; dropped, nothing does.
    let err = format!("{:#}", s.remove(&["et/carpetx"], remove_yes()).unwrap_err());
    assert!(err.contains("Other packages depend on et/carpetx"), "{err}");
    claims(&s, "claims override drop max/z4c:dependency:et/carpetx").unwrap();
    s.remove(&["max/z4c"], remove_yes()).unwrap();

    // A dependency the manifest lacks.
    s.add(&["max/plain", "et/carpetx/Arith"], yes()).unwrap();
    s.remove(&["et/carpetx"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    s.add(&["et/carpetx/Arith"], yes()).unwrap();
    claims(&s, "claims override add max/plain --dependency et/carpetx@^1.0 --needs Arith --needed-by Plain").unwrap();
    assert!(overrides(&s).contains(&"add max/plain dependency et/carpetx@^1.0 needs Arith needed-by Plain".to_owned()));
    let err = format!("{:#}", s.remove(&["et/carpetx"], remove_yes()).unwrap_err());
    assert!(err.contains("Other packages depend on et/carpetx") && err.contains("max/plain@1.0.0"), "{err}");
}

/// What an override names must exist, and the refusal says what does not.
#[test]
fn an_override_must_name_what_exists() {
    let s = overridable(true);
    let refused = |words: &str| match words.split_once("--remove ") {
        Some((head, key)) => {
            let words: Vec<&str> = head.split_whitespace().chain(["--remove", key]).collect();
            format!("{:#}", claims_args(&s, &words).unwrap_err())
        }
        None => format!("{:#}", claims(&s, words).unwrap_err()),
    };
    assert_eq!(refused("claims override drop max/nothere:dependency:et/bar"), "max/nothere is unknown to Cactarium.");
    assert_eq!(
        refused("claims override drop max/z4c:dependency:et/arith"),
        "max/z4c makes no dependency claim on et/arith in any version."
    );
    assert_eq!(
        refused("claims override drop max/z4c:fork-of:et/carpetx"),
        "max/z4c makes no fork-of claim on et/carpetx in any version."
    );
    assert_eq!(refused("claims override needs max/z4c:dependency:et/arith Loop"), "max/z4c has no dependency on et/arith in any version.");
    assert_eq!(
        refused("claims override needs max/z4c:dependency:et/carpetx Nothere"),
        "et/carpetx has no thorn Nothere in any version."
    );
    assert!(refused("claims override add max/z4c --dependency et/carpetx --needs Loop").contains("already depends on et/carpetx"));
    assert_eq!(
        refused("claims override add max/plain --dependency et/carpetx --needs Nothere"),
        "et/carpetx has no thorn Nothere in any version."
    );
    assert_eq!(
        refused("claims override add max/plain --dependency et/carpetx --needs Loop --needed-by Nothere"),
        "max/plain has no thorn Nothere in any version."
    );
    assert_eq!(refused("claims override satisfies max/carpetx-mp et/carpetx"), "max/carpetx-mp is unknown to Cactarium.");
    assert_eq!(refused("claims override satisfies local:nothere et/carpetx"), "The installation has no repo nothere.");
    assert!(refused("claims override satisfies max/plain max/plain").contains("cannot stand in for itself"));
    assert!(refused("claims override drop max/z4c").contains("OWNER/PKG:KIND:TARGET"));
    assert!(refused("claims override --remove drop max/z4c:dependency:et/carpetx").contains("No override"));
    assert!(overrides(&s).is_empty());

    // Setting one twice says so, and keeps one.
    claims(&s, "claims override satisfies local:basecx et/carpetx").unwrap();
    claims(&s, "claims override satisfies local:basecx et/carpetx").unwrap();
    assert_eq!(overrides(&s).len(), 1);
}

/// The record is read before every resolution: one that does not read refuses the command rather
/// than running without it, and an override changed while a prompt waits refuses the add.
#[test]
fn a_record_that_does_not_read_refuses_and_a_change_under_the_prompt_is_caught() {
    let s = overridable(false);
    let mut meta = s.meta();
    meta.claim_overrides.insert("prefer et/a".into(), crate::installation::ClaimOverride { kind: "drop".into(), note: None });
    s.write_meta(&meta);
    let err = format!("{:#}", s.add(&["max/plain"], yes()).unwrap_err());
    assert!(err.contains("installation.toml has a claim override that does not read: 'prefer et/a'.\nRemove it with `cactup claims override --remove 'prefer et/a'`."), "{err}");
    assert_eq!(err.matches("prefer et/a").count(), 2, "{err}");
    assert!(claims(&s, "claims list").is_err());
    meta.claim_overrides.clear();
    s.write_meta(&meta);

    let start = Start::read(&s.ctx, false, Doing::Adding).unwrap();
    claims(&s, "claims override drop max/app:incompatible-with:max/solo").unwrap();
    let locked = start.inst.locked().unwrap();
    let err = format!("{:#}", start.recheck(&locked).unwrap_err());
    assert!(err.contains("changed while"), "{err}");
}

/// A range with spaces or `||` is written as one word that reads back, so the installation stays
/// usable; a track the target lacks, a range no version meets and a package depending on itself
/// are refused, and so is a note over two lines.
#[test]
fn a_dependency_override_is_written_to_read_back_and_must_be_one_that_can_be_met() {
    let s = overridable(false);
    for (range, key) in [
        ("et/carpetx@>=1.0, <2.0", "add max/plain dependency et/carpetx@>=1.0,<2.0 needs Arith"),
        ("et/carpetx@1.x || 2.x", "add max/plain dependency et/carpetx@1.*||2.* needs Arith"),
    ] {
        claims_args(&s, &["claims", "override", "add", "max/plain", "--dependency", range, "--needs", "Arith"])
            .unwrap_or_else(|e| panic!("{e:#}"));
        assert!(overrides(&s).contains(&key.to_owned()), "{:?}", overrides(&s));
        s.add(&["max/solo"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
        s.remove(&["max/solo"], remove_yes()).unwrap();
        claims(&s, "claims list").unwrap();
        claims_args(&s, &["claims", "override", "--remove", key]).unwrap();
    }
    let refused = |dependency: &str| {
        let words = ["claims", "override", "add", "max/plain", "--dependency", dependency, "--needs", "Arith"];
        format!("{:#}", claims_args(&s, &words).unwrap_err())
    };
    assert_eq!(refused("et/carpetx@bogus"), "et/carpetx has no version on the track bogus.");
    assert_eq!(refused("et/carpetx@^9"), "No version of et/carpetx meets ^9.");
    assert_eq!(refused("et/carpetx@:^1||mixed:^1"), "et/carpetx has no version on the track mixed.");
    assert_eq!(refused("max/plain"), "max/plain cannot depend on itself.");
    assert!(overrides(&s).is_empty());
    // Declared in some versions of a package that is not installed: allowed; in the installed one: not.
    s.add(&["max/two@1.0.0"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    claims(&s, "claims override add max/two --dependency et/carpetx --needs Arith").unwrap();
    // Not installed: the versions that do not declare it leave room.
    claims(&overridable(false), "claims override add max/two --dependency et/carpetx --needs Arith").unwrap();
    let other = overridable(false);
    other.add(&["max/two"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let err = format!("{:#}", claims(&other, "claims override add max/two --dependency et/carpetx --needs Loop").unwrap_err());
    assert!(err.contains("max/two already depends on et/carpetx."), "{err}");
}

/// What `claims check` could not check is said, with why, and "No problems." is never said beside it.
#[test]
fn check_says_what_it_could_not_check() {
    let (s, fake) = overridable_with_fake(true);
    let meta = s.meta();
    let found = crate::commands::claims::problems(&s.ctx, &s.inst, &meta).unwrap();
    assert!(found.lines.is_empty() && found.not_checked().is_empty(), "{:?}", found.not_checked());

    // An added package Cactarium can no longer give: its dependencies are unknown.
    s.add(&["max/z4c"], AddFlags { yes: true, ..AddFlags::default() }).unwrap_err();
    claims(&s, "claims override satisfies local:basecx et/carpetx").unwrap();
    s.add(&["max/z4c"], yes()).unwrap();
    fake.lock().unwrap().packages.retain(|(p, _)| p != "max/z4c");
    let found = crate::commands::claims::problems(&s.ctx, &s.inst, &s.meta()).unwrap();
    let said = found.not_checked();
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].starts_with("Not checked: the dependencies of these packages are unknown:\n  max/z4c@1.0.0: "), "{said:?}");

    // No fetch record (an old installation): no base repo is mapped to a package.
    std::fs::remove_file(s.inst.cactup_dir().join("fetch-state.toml")).unwrap();
    let found = crate::commands::claims::problems(&s.ctx, &s.inst, &s.meta()).unwrap();
    let said = found.not_checked();
    assert!(said.iter().any(|l| l.starts_with("Not checked: no base repo is mapped to a package")), "{said:?}");
}

/// An unmet CCL requirement names the providers; on a Cactarium that cannot list them, it says so.
#[test]
fn check_says_when_cactarium_cannot_list_providers() {
    let (s, fake) = overridable_with_fake(false);
    s.add(&["max/plain"], yes()).unwrap();
    let ccl = s.inst.root.join("Cactus/repos/plain/Plain/interface.ccl");
    std::fs::write(&ccl, "IMPLEMENTS: plain\nINHERITS: grid\n").unwrap();
    let lines = |s: &Setup| crate::commands::claims::problems(&s.ctx, &s.inst, &s.meta()).unwrap().lines;
    assert_eq!(
        lines(&s),
        ["Plain (max/plain@1.0.0) inherits or befriends grid; no installed thorn provides it. No registered package provides it."]
    );
    fake.lock().unwrap().providers_endpoint = false;
    assert_eq!(
        lines(&s),
        ["Plain (max/plain@1.0.0) inherits or befriends grid; no installed thorn provides it. Cactarium cannot list providers."]
    );
}

/// `claims list` marks an override that does not read or names a repo the base lacks, and the
/// record can be removed by its key.
#[test]
fn list_marks_what_is_wrong_with_an_override() {
    let s = overridable(true);
    let mut meta = s.meta();
    let record = |kind: &str, note: Option<&str>| crate::installation::ClaimOverride { kind: kind.into(), note: note.map(Into::into) };
    meta.claim_overrides.insert("satisfies local:basecx et/carpetx".into(), record("satisfies", Some("kept")));
    meta.claim_overrides.insert("satisfies local:gone et/carpetx".into(), record("satisfies", None));
    meta.claim_overrides.insert("prefer et/a".into(), record("prefer", None));
    let repos: std::collections::BTreeSet<String> = ["basecx".to_owned()].into();
    let (lines, bad) = crate::commands::claims::override_lines(&meta, Some(&repos));
    assert_eq!(
        lines,
        [
            "Claim overrides:",
            "  prefer et/a  (does not read)",
            "  satisfies local:basecx et/carpetx  (kept)",
            "  satisfies local:gone et/carpetx  (the installation has no repo gone)",
        ]
    );
    assert_eq!(bad.as_deref(), Some("prefer et/a"));
    s.write_meta(&meta);
    let err = format!("{:#}", claims(&s, "claims list").unwrap_err());
    assert!(err.starts_with("installation.toml has a claim override that does not read: 'prefer et/a'."), "{err}");
    // The check flags the stale one in the installation itself.
    claims_args(&s, &["claims", "override", "--remove", "prefer et/a"]).unwrap();
    claims(&s, "claims list").unwrap();
    let found = crate::commands::claims::problems(&s.ctx, &s.inst, &s.meta()).unwrap();
    assert_eq!(found.lines.len(), 1, "{:?}", found.lines);
    assert!(found.lines[0].starts_with("The override 'satisfies local:gone et/carpetx' names a repo that is not installed."));
}

// ---- Stack releases: frozen, so their packages' claims are checked and only warned about ----

/// `inst refetch --release` to a stack whose packages conflict (max/app is incompatible with
/// max/solo): the refetch goes through, and the check's problem comes out as a warning naming the
/// way past. With Cactarium unable to say what the base is, the check says it did not run, and
/// still fails nothing.
#[test]
fn a_refetch_to_a_stack_warns_of_claim_problems_without_failing() {
    let tmp = tempfile::tempdir().unwrap();
    let (aurl, a) = upstream(tmp.path(), "app", &[("App/interface.ccl", "IMPLEMENTS: app\n")]);
    let (surl, so) = upstream(tmp.path(), "solo", &[("Solo/interface.ccl", "IMPLEMENTS: solo\n")]);
    let hates = r#"{"kind":"incompatible-with","target":"max/solo","range":null,"needs":[],"note":null}"#;
    let list = format!("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{}{}", section(&aurl, &a, &["App/App"]), section(&surl, &so, &["Solo/Solo"]));
    let frozen = serde_json::json!({
        "format": 1, "stack": "max/stack@1.0.0", "root": "Cactus",
        "repo": [{"name": "app", "url": aurl, "commit": a}, {"name": "solo", "url": surl, "commit": so}],
        "thornlist": {"filename": "stack.th", "text": list},
    });
    let mut fake = Fake::new()
        .package("max/app", vec![version("1.0.0", &a, &aurl, "App", &["App"], &format!("[{hates}]"))])
        .package("max/solo", vec![version("1.0.0", &so, &surl, "Solo", &["Solo"], "[]")])
        .pin(&aurl, "max/app", None)
        .pin(&surl, "max/solo", None);
    fake.stacks.push(("max/stack".into(), frozen.to_string()));
    let (cactarium, log, fake) = fake.serve_shared();
    let s = Setup::new(tmp, &section(&aurl, &a, &["App/App"]), &cactarium);
    let refetch = |s: &Setup| {
        let argv = ["cactup", "inst", "refetch", "--release", "max/stack@1.0.0"];
        let args = match <crate::args::Args as clap::Parser>::try_parse_from(argv).unwrap().command {
            crate::args::Commands::Inst(crate::args::InstallationCommand::Refetch(args)) => args,
            other => panic!("{other:?}"),
        };
        super::super::refetch::dispatch(&s.ctx, args)
    };
    refetch(&s).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.inst.root.join("Cactus/arrangements/Solo/Solo/interface.ccl").is_file(), "the stack is installed");
    let asked: Vec<String> = log.lock().unwrap().iter().map(|(p, _)| p.clone()).collect();
    let at = asked.iter().position(|p| p.contains("/stacks/max/stack/releases/1.0.0")).unwrap();
    assert!(asked[at..].iter().any(|p| p == "/api/v1/lookup"), "checked after the fetch: {asked:?}");
    let said = crate::commands::claims::warn_problems(&s.ctx, &s.inst);
    assert_eq!(
        said,
        [
            "Claim problems:",
            "  max/app@1.0.0 is incompatible with max/solo@1.0.0. Run `cactup claims override drop max/app:incompatible-with:max/solo`.",
        ]
    );

    fake.lock().unwrap().lookup = false;
    refetch(&s).unwrap_or_else(|e| panic!("{e:#}"));
    let said = crate::commands::claims::warn_problems(&s.ctx, &s.inst);
    assert!(said[0].starts_with("Did not check the claims: "), "{said:?}");
    assert_eq!(said.last().unwrap(), "Check them with `cactup claims check`.");
}

/// The warnings after a stack install are bounded: a Cactarium slower than their deadline is
/// said not to have answered in time; a Ctrl-C while they run ends them alone (the install
/// that came before stays a success: `interrupt_taken_by_warnings`); a Ctrl-C from before
/// they start leaves them unsaid. The deadline is shortened, and the interrupt flag is the
/// process's: run in a child of its own.
#[test]
fn the_warnings_after_a_stack_install_are_bounded() {
    if std::env::var_os("CACTUP_TEST_WARN_DEADLINE_MS").is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["commands::add::deps_tests::the_warnings_after_a_stack_install_are_bounded", "--exact", "--test-threads=1"])
            .env("CACTUP_TEST_WARN_DEADLINE_MS", "500")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && said.contains("1 passed"), "{said}{}", String::from_utf8_lossy(&out.stderr));
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (aurl, a) = upstream(tmp.path(), "app", &[("App/interface.ccl", "IMPLEMENTS: app\n")]);
    let mut fake = Fake::new()
        .package("max/app", vec![version("1.0.0", &a, &aurl, "App", &["App"], "[]")])
        .pin(&aurl, "max/app", None);
    fake.lookup_delay = std::time::Duration::from_secs(30);
    let (cactarium, _, _) = fake.serve_shared();
    let s = Setup::new(tmp, &section(&aurl, &a, &["App/App"]), &cactarium);

    let started = std::time::Instant::now();
    let said = crate::commands::claims::warn_problems(&s.ctx, &s.inst);
    assert!(started.elapsed() < std::time::Duration::from_secs(4), "{:?}", started.elapsed());
    assert_eq!(said, ["Did not check the claims: Cactarium did not answer in time.", "Check them with `cactup claims check`."]);
    assert!(!crate::commands::claims::interrupt_taken_by_warnings());

    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(150));
        gix::interrupt::IS_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let started = std::time::Instant::now();
    let said = crate::commands::claims::warn_problems(&s.ctx, &s.inst);
    assert!(started.elapsed() < std::time::Duration::from_millis(450), "ended by Ctrl-C, not the deadline: {:?}", started.elapsed());
    assert_eq!(said, ["Did not check the claims: interrupted.", "Check them with `cactup claims check`."]);
    assert!(crate::commands::claims::interrupt_taken_by_warnings(), "the finished install stays a success");

    // A Ctrl-C from before: nothing is checked or said.
    assert!(crate::commands::claims::warn_problems(&s.ctx, &s.inst).is_empty());
}

/// `sim pack -I` of a thornlist with a repo the installation never fetched: refused for it,
/// naming `cactup inst refetch`, rather than written unpinned.
#[test]
fn pack_of_a_repo_the_installation_never_fetched_is_refused() {
    use crate::fetch::anonymous::testserver::{serve, Mode};
    let tmp = tempfile::tempdir().unwrap();
    serve("unfetched.pack.test", Mode::Public);
    let (aurl, a) = upstream(tmp.path(), "app", &[("App/interface.ccl", "IMPLEMENTS: app\n")]);
    let (cactarium, _, _) = Fake::new().package("max/app", vec![version("1.0.0", &a, &aurl, "App", &["App"], "[]")]).serve_shared();
    let s = Setup::new(tmp, &section(&aurl, &a, &["App/App"]), &cactarium);
    serve("private.pack.test", Mode::Private);
    let list = format!(
        "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{}\n!TARGET = $ROOT/arrangements\n!TYPE = git\n\
         !URL = https://unfetched.pack.test/o/nofetch.git\n!REPO_PATH = $2\n!CHECKOUT = Nofetch/One\n\
         !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://TOKEN@unfetched.pack.test/o/tok.git\n\
         !REPO_PATH = $2\n!CHECKOUT = Tok/One\n\
         !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = git@private.pack.test:o/priv.git\n\
         !REPO_PATH = $2\n!CHECKOUT = Priv/One\n",
        section(&aurl, &a, &["App/App"])
    );
    let th = s.inst.root.join("pack.th");
    std::fs::write(&th, &list).unwrap();
    let par = s.inst.root.join("run.par");
    std::fs::write(&par, "ActiveThorns = \"App\"\n").unwrap();
    let ctx = Ctx {
        globals: crate::args::GlobalOpts { installation: Some("t".into()), ..s.ctx.globals.clone() },
        db: s.ctx.db.clone(),
    };
    let args = crate::args::SimPackArgs {
        thornlist: th,
        parfile: par.clone(),
        file: Some(s.inst.root.join("out.sim")),
        describe: crate::args::SimDescribeArgs { force: true, yes: true, ..Default::default() },
        trim: false,
    };
    let err = format!("{:#}", crate::sim::export::pack(&ctx, args).unwrap_err());
    let shown = s.inst.root.join("pack.th").display().to_string();
    let hint = format!("\n  nofetch: not in the installation's thornlist; fetch it with `cactup inst refetch {shown}`\n");
    assert!(err.contains(&hint), "{err}");
    // A repo with a URL refused is said once, for its URL.
    assert!(err.contains("\n  tok: https://***@unfetched.pack.test/o/tok.git carries"), "{err}");
    assert!(err.contains("\n  priv: git@private.pack.test:o/priv.git is an ssh URL"), "{err}");
    assert_eq!(err.matches("\n  tok: ").count() + err.matches("\n  priv: ").count(), 2, "{err}");
    assert!(!s.inst.root.join("out.sim").exists());

    // The hint works: a refetch from the packed list fetches a repo the
    // installation's list lacked, and pack then pins it.
    let (lurl, l) = upstream(s.inst.root.parent().unwrap(), "late", &[("Late/interface.ccl", "IMPLEMENTS: late\n")]);
    let list = format!("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{}{}", section(&aurl, &a, &["App/App"]), section(&lurl, &l, &["Late/Late"]));
    let th = s.inst.root.join("late.th");
    std::fs::write(&th, &list).unwrap();
    let pack_local = |th: &std::path::Path| {
        let args = crate::args::SimPackArgs {
            thornlist: th.to_owned(),
            parfile: par.clone(),
            file: Some(s.inst.root.join("out.sim")),
            describe: crate::args::SimDescribeArgs { force: true, yes: true, local: true, ..Default::default() },
            trim: false,
        };
        crate::sim::export::pack(&ctx, args).unwrap_or_else(|e| panic!("{e:#}"));
        let text = std::fs::read_to_string(s.inst.root.join("out.sim")).unwrap();
        cactup_ecosystem_common::sim::SimFile::from_toml(&text).unwrap()
    };
    let late = |file: &cactup_ecosystem_common::sim::SimFile| file.repos.iter().find(|r| r.name == "late").unwrap().commit.clone();
    assert_eq!(late(&pack_local(&th)), None, "not fetched yet");
    let argv = ["cactup", "inst", "refetch", th.to_str().unwrap()];
    let args = match <crate::args::Args as clap::Parser>::try_parse_from(argv).unwrap().command {
        crate::args::Commands::Inst(crate::args::InstallationCommand::Refetch(args)) => args,
        other => panic!("{other:?}"),
    };
    super::super::refetch::dispatch(&s.ctx, args).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(late(&pack_local(&th)), Some(l.clone()));
}
