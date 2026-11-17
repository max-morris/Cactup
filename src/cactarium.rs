//! The Cactarium registry's HTTP API, from cactup's side (spec §8.10). The
//! request and response shapes are the shared crate's (`api`), the very
//! structs the server serves, so the two cannot drift apart silently; this
//! module is only the transport: where the registry is, the token, the
//! generation check, and turning an error body into the message it carries.
//!
//! Everything here runs on the login node, before anything is built or
//! submitted; nothing on a compute node's path calls it (D11).

use crate::database::Database;
use crate::format::{self, Format, Toml};
use crate::Res;
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use cactup_ecosystem_common::api::{
    ErrorBody, LookedUp, LookupRequest, PackageVersion, PackageVersions, PinInfo, PinQuery, Providers,
    ProvidersQuery, Published, Refreshed, Resolved, StackReleases, VersionInfo, API_GENERATION,
    API_GENERATION_HEADER, API_PREFIX, CONTACT_HEADER, MAX_LOOKUP_PINS, SHA256_HEADER, SIM_QUERY,
    UNPINNED_HEADER, UNTRUSTED_HEADER,
};
use cactup_ecosystem_common::stack::FrozenStack;
use cactup_ecosystem_common::STACK_FORMAT;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

/// Where the registry is when nothing says otherwise.
pub const DEFAULT_URL: &str = "https://cactarium.cct.lsu.edu";

/// The knob naming another registry (a test instance, a mirror).
pub const URL_KNOB: &str = "cactarium-url";

/// Check a `cactarium-url` value: an `http(s)://host[:port][/path]` URL with
/// no query or fragment, stored without a trailing slash. Plain http is
/// accepted for a test instance on a private network.
pub fn validate_url(value: &str) -> Res<String> {
    let url = value.trim().trim_end_matches('/');
    let parsed = (!url.contains(char::is_whitespace)).then(|| reqwest::Url::parse(url).ok()).flatten();
    match parsed {
        Some(p)
            if matches!(p.scheme(), "http" | "https")
                && p.host_str().is_some_and(|h| !h.is_empty())
                && p.query().is_none()
                && p.fragment().is_none() =>
        {
            Ok(url.to_owned())
        }
        _ => bail!("Invalid {URL_KNOB} \"{value}\": give the registry's address, like {DEFAULT_URL}"),
    }
}

/// How long a request made to resolve dependencies may wait for the head of
/// its answer (connecting, sending, the server's own work: a cold read of a
/// package at a commit fetches its repository) and for each further piece of
/// the body. Generous: Ctrl-C ends any wait at once ([`interruptibly`]).
const WAIT: std::time::Duration = std::time::Duration::from_secs(120);
/// How long connecting may take, within [`WAIT`].
const CONNECT: std::time::Duration = std::time::Duration::from_secs(30);

/// The HTTP client for resolving dependencies ([`Client::patient`]):
/// cactup's `User-Agent`, and [`WAIT`] on the head of each answer and on each
/// stall of its body, rather than reqwest's 30 s default (blocking reqwest's
/// timeout bounds the whole wait for the head). Every other request uses the
/// shared download client and its 30 s.
static PATIENT: std::sync::LazyLock<reqwest::blocking::Client> = std::sync::LazyLock::new(|| {
    reqwest::blocking::Client::builder()
        .user_agent(crate::build_info::USER_AGENT)
        .timeout(WAIT)
        .connect_timeout(CONNECT)
        .build()
        .expect("failed to build the HTTP client")
});

/// Ctrl-C while a request was in flight: it may have reached the server.
#[derive(Debug)]
pub(crate) struct InFlight;

impl std::fmt::Display for InFlight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Interrupted")
    }
}

impl std::error::Error for InFlight {}

/// Runs `request` on a thread of its own and waits for it, checking `stop`
/// every 50 ms: once it is set, fails with "interrupted" at once and leaves
/// the request to finish (or time out) on its own. Blocking reqwest cannot be
/// cancelled, and an answer can take a while on a slow link.
pub(crate) fn interruptibly<T: Send + 'static>(
    request: impl FnOnce() -> Res<T> + Send + 'static,
    stop: &std::sync::atomic::AtomicBool,
) -> Res<T> {
    // Not started at all once Ctrl-C was pressed.
    if stop.load(std::sync::atomic::Ordering::Relaxed) {
        bail!("Interrupted");
    }
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(request());
    });
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(anyhow::Error::new(InFlight));
        }
        match receive.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(answer) => return answer,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => bail!("A request to the Cactarium died"),
        }
    }
}

/// A connection to one registry: its base URL and, for writes, the token.
#[derive(Clone, Debug)]
pub struct Client {
    base: String,
    /// A limit on each whole request, the answer's body included (reqwest's
    /// per-request timeout): set for lookups a command makes in passing (`inst
    /// show`), so a registry that cannot be reached, or answers slowly, costs
    /// seconds. Without it, the client's own waits bound each wait.
    timeout: Option<std::time::Duration>,
    /// Waits as resolving dependencies may ([`PATIENT`]).
    patient: bool,
    /// What interrupts a request in flight: Ctrl-C (a test's own flag).
    stop: &'static std::sync::atomic::AtomicBool,
}

impl Client {
    /// The registry `CACTARIUM_URL` names, else the `cactarium-url` knob,
    /// else the public one.
    pub fn new(db: &Database) -> Client {
        let base = std::env::var("CACTARIUM_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .or_else(|| db.knob(URL_KNOB).map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_URL.to_owned());
        Client::at(&base)
    }

    pub fn at(base: &str) -> Client {
        Client {
            base: base.trim_end_matches('/').to_owned(),
            timeout: None,
            patient: false,
            stop: &gix::interrupt::IS_INTERRUPTED,
        }
    }

    /// This client, waiting as long as resolving dependencies may: a read of
    /// a package at a commit can make the server fetch its repository.
    pub fn patient(self) -> Client {
        Client { patient: true, ..self }
    }

    /// This client, stopped by `stop` rather than Ctrl-C (tests).
    #[cfg(test)]
    pub fn stopped_by(self, stop: &'static std::sync::atomic::AtomicBool) -> Client {
        Client { stop, ..self }
    }

    fn http(&self) -> &'static reqwest::blocking::Client {
        if self.patient { &PATIENT } else { crate::fetch::download::client() }
    }

    /// This client, giving up on any request, its whole answer read, after
    /// `timeout`.
    pub fn with_timeout(self, timeout: std::time::Duration) -> Client {
        Client { timeout: Some(timeout), ..self }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{API_PREFIX}{path}", self.base)
    }

    /// The newest release (or `@head`/`@master`, or an exact version)
    /// matching `req`, which is `owner/name[@…]` as a user writes it.
    /// `kind` is the kind of entry the caller wants (`package`, `stack` or
    /// `sim`): a Cactarium that knows the parameter refuses an entry of
    /// another kind saying so, before reading the rest of the reference; an
    /// older one ignores it, and the caller's own check of `Resolved::kind`
    /// still holds.
    pub fn resolve(&self, req: &str, kind: &str) -> Res<Resolved> {
        let url = format!("{}?req={}&kind={}", self.url("/resolve"), query_escape(req), query_escape(kind));
        self.send(self.http().get(&url))?.json()
    }

    /// A stack release (`version` exact, or `head`/`master`), with what the
    /// server's headers say about it.
    pub fn stack_release(&self, owner: &str, name: &str, version: &str) -> Res<StackResponse> {
        let url = self.url(&format!("/stacks/{owner}/{name}/releases/{version}"));
        let response = self.send(self.http().get(&url))?;
        let flag = |header: &str| response.headers().get(header).map(|v| v.as_bytes() == b"true");
        let unpinned = flag(UNPINNED_HEADER).unwrap_or(false);
        // A tip without the trust header (a registry that predates it, or
        // left it off) is taken as untrusted: the check fails closed.
        let tip = matches!(version, "head" | "master");
        let untrusted = flag(UNTRUSTED_HEADER).unwrap_or(unpinned || tip);
        let contact = response
            .headers()
            .get(CONTACT_HEADER)
            .and_then(|v| v.to_str().ok())
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        let url = response.url.clone();
        Ok(StackResponse { stack: stack_json(response.text()?, &url)?, unpinned, untrusted, contact })
    }

    pub fn package_versions(&self, owner: &str, name: &str) -> Res<PackageVersions> {
        let url = self.url(&format!("/packages/{owner}/{name}/versions"));
        self.send(self.http().get(&url))?.json()
    }

    /// [`Client::package_versions`], or `None` when the Cactarium has no
    /// such package.
    pub fn package_versions_if_any(&self, owner: &str, name: &str) -> Res<Option<PackageVersions>> {
        let url = self.url(&format!("/packages/{owner}/{name}/versions"));
        self.send_found(self.http().get(&url), &[reqwest::StatusCode::NOT_FOUND])?.ok().map(Answer::json).transpose()
    }

    /// A package read at something no published version names: a branch
    /// tip (`head` or `master`: both its default branch) or a commit. Its
    /// thorns there, and whether the Cactarium does not trust it (then
    /// nobody has reviewed that code); `None` when the Cactarium has no such
    /// package or cannot find that commit in its repository.
    pub fn package_at_if_any(&self, owner: &str, name: &str, at: &str) -> Res<Option<(PackageVersion, bool)>> {
        let url = self.url(&format!("/packages/{owner}/{name}/versions/{at}"));
        let Ok(response) = self.send_found(self.http().get(&url), &[reqwest::StatusCode::NOT_FOUND])? else {
            return Ok(None);
        };
        let untrusted = response.headers().get(UNTRUSTED_HEADER).is_none_or(|v| v.as_bytes() == b"true");
        Ok(Some((response.json()?, untrusted)))
    }

    /// Which package, if any, each pinned repo is, in the order asked. Every
    /// pin goes in one request (the endpoint is rate-limited per address), or
    /// in as few as its size limit allows; each request is checked as the
    /// server checks it before it is sent.
    pub fn lookup(&self, pins: &[PinQuery]) -> Res<Vec<PinInfo>> {
        let mut out = Vec::with_capacity(pins.len());
        for chunk in pins.chunks(MAX_LOOKUP_PINS) {
            if gix::interrupt::is_triggered() {
                bail!("Interrupted");
            }
            let request = LookupRequest { pins: chunk.to_vec(), downloads: Vec::new() };
            request.validate().map_err(|e| anyhow!("Cannot ask the Cactarium about these repos: {e}"))?;
            let body = serde_json::to_vec(&request).context("Failed to encode the lookup request")?;
            let request = self.http()
                .post(self.url("/lookup"))
                .header("content-type", "application/json")
                .body(body);
            let response = self.send_found(request, MISSING_ROUTE)?.map_err(|m| self.missing("/lookup", m))?;
            let answer: LookedUp = response.json()?;
            if answer.pins.len() != chunk.len() {
                bail!(
                    "The Cactarium answered a lookup of {} repos with {}; cactup cannot tell which is which",
                    chunk.len(),
                    answer.pins.len()
                );
            }
            for (asked, info) in chunk.iter().zip(&answer.pins) {
                // The answer echoes each pin as asked; a server that spells
                // the URL its own way still names the same repo, as the
                // registry compares them.
                let same = cactup_ecosystem_common::url::same_repo_lenient(&info.url, &asked.url);
                if !same || !info.commit.eq_ignore_ascii_case(&asked.commit) {
                    bail!(
                        "The Cactarium answered a lookup of {} at {} with {} at {}, out of order",
                        asked.url,
                        asked.commit,
                        info.url,
                        info.commit
                    );
                }
            }
            out.extend(answer.pins);
        }
        Ok(out)
    }

    /// The registered thorns that meet a CCL requirement: for each package
    /// and track, its newest version's.
    pub fn providers(&self, need: &cactup_ecosystem_common::ccl::Need) -> Res<Providers> {
        let query = ProvidersQuery::of(need)
            .to_query_string()
            .map_err(|e| anyhow!("Cannot ask the Cactarium what provides {need}: {e}"))?;
        let url = format!("{}?{query}", self.url("/providers"));
        let response = self.send_found(self.http().get(&url), MISSING_ROUTE)?.map_err(|m| self.missing("/providers", m))?;
        response.json()
    }

    /// A route `endpoint` the server does not have. Only for endpoints that
    /// never answer 404 about what they are asked (a lookup answers every
    /// pin, a provider search an empty list): their 404 (or 405) can only
    /// mean the route is missing. A Cactarium says so with its API
    /// generation, as every answer under its API carries one (its fallback
    /// route too): then it is older than this cactup. Without one, the
    /// address is no Cactarium's.
    fn missing(&self, endpoint: &'static str, m: Missing) -> anyhow::Error {
        if m.generation {
            return anyhow::Error::new(OldCactarium { base: self.base.clone(), endpoint });
        }
        anyhow!(
            "The server at {} does not look like a Cactarium: it answered {} to {endpoint}, without saying which \
             Cactarium API it speaks. Check the {URL_KNOB} knob (or CACTARIUM_URL)",
            self.base,
            m.status
        )
    }

    /// Every release of a stack, each with whether it has been yanked.
    pub fn stack_releases(&self, owner: &str, name: &str) -> Res<StackReleases> {
        let url = self.url(&format!("/stacks/{owner}/{name}/releases"));
        self.send(self.http().get(&url))?.json()
    }

    /// Whether `version` of package or stack `owner/name` has been yanked
    /// since it was installed: `Some(reason)` if so (the reason may be
    /// empty), `None` if not, or if the registry no longer lists it.
    pub fn yanked_since(&self, kind: EntryKind, owner: &str, name: &str, version: &str) -> Res<Option<String>> {
        let info: Option<VersionInfo> = match kind {
            EntryKind::Package => self
                .package_versions(owner, name)?
                .versions
                .into_iter()
                .map(|v| v.version)
                .find(|v| v.version == version),
            EntryKind::Stack => self.stack_releases(owner, name)?.releases.into_iter().find(|v| v.version == version),
        };
        Ok(info.filter(|v| v.yanked).map(|v| v.yank_reason.unwrap_or_default()))
    }

    /// Download sim file `owner/name@number` to `dest`, checked against the
    /// hash the server sends with it. `sim` names one sim of a multi-sim
    /// entry (`?sim=`, which such an entry requires); `None` for an entry
    /// that is one sim. One sim of several must also be stamped as that sim
    /// (`owner/name/sim@number`). Nothing is written to `dest` unless every
    /// check passes.
    pub fn download_sim(&self, owner: &str, name: &str, version: &str, sim: Option<&str>, dest: &Path) -> Res<()> {
        let mut url = self.url(&format!("/sims/{owner}/{name}/{version}/download"));
        if let Some(sim) = sim {
            url.push_str(&format!("?{SIM_QUERY}={}", query_escape(sim)));
        }
        let response = self.send(self.http().get(&url))?;
        let expected = response.headers().get(SHA256_HEADER).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let bytes = response.bytes().with_context(|| format!("Failed to download {url}"))?;
        if let Some(expected) = expected {
            let actual = sha256_hex(&bytes);
            if !actual.eq_ignore_ascii_case(&expected) {
                let what = match sim {
                    Some(sim) => format!("{owner}/{name}/{sim}@{version}"),
                    None => format!("{owner}/{name}@{version}"),
                };
                bail!("The download of {what} has SHA-256 {actual}, but the registry says {expected}");
            }
        }
        if let Some(sim) = sim {
            check_stamp(&bytes, &format!("{owner}/{name}/{sim}@{version}"))?;
        }
        let dir = dest.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("Failed to create '{}'", dir.display()))?;
        // Whole or not at all: the cached copy is used again, offline too.
        let mut temp = tempfile::NamedTempFile::new_in(dir).with_context(|| format!("Failed to write '{}'", dest.display()))?;
        std::io::Write::write_all(&mut temp, &bytes).with_context(|| format!("Failed to write '{}'", dest.display()))?;
        temp.persist(dest).with_context(|| format!("Failed to write '{}'", dest.display()))?;
        Ok(())
    }

    /// Publish a `.sim` file's bytes as version `version` of sim
    /// `owner/name` (`X.Y.Z`), or, without one, as the next major version.
    pub fn publish_sim(&self, owner: &str, name: &str, body: Vec<u8>, version: Option<&str>) -> Res<Published> {
        let token = token_for(&self.base)?;
        let path = match version {
            Some(v) => format!("/sims/{owner}/{name}?version={}", query_escape(v)),
            None => format!("/sims/{owner}/{name}"),
        };
        let request = self.http()
            .post(self.url(&path))
            .bearer_auth(token)
            .header("content-type", "application/toml")
            .body(body);
        self.send_change(request, "the publication")?.json()
    }

    /// Publish a stack release.
    pub fn publish_stack(
        &self,
        owner: &str,
        name: &str,
        request: &cactup_ecosystem_common::api::ReleaseRequest,
    ) -> Res<Published> {
        let token = token_for(&self.base)?;
        let body = serde_json::to_vec(request).context("Failed to encode the release request")?;
        let request = self.http()
            .post(self.url(&format!("/stacks/{owner}/{name}/releases")))
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body(body);
        self.send_change(request, "the publication")?.json()
    }

    /// Have the registry check package (or repo-kept stack) `owner/name`'s
    /// repository for new release tags now, rather than at its next poll.
    pub fn refresh(&self, owner: &str, name: &str) -> Res<Refreshed> {
        let token = token_for(&self.base)?;
        let request = self.http()
            .post(self.url(&format!("/packages/{owner}/{name}/refresh")))
            .bearer_auth(token);
        self.send_change(request, "the refresh")?.json()
    }

    /// A request that never got an answer. A server that cannot be reached
    /// (no such host, refused, timed out) is one line naming it, its root
    /// cause, and how to point cactup elsewhere; reqwest's chain of wrappers
    /// says nothing more. Anything else keeps its full chain.
    fn unreachable(&self, e: reqwest::Error) -> anyhow::Error {
        if !(e.is_connect() || e.is_timeout()) {
            return anyhow::Error::new(e).context(format!("Failed to reach the Cactarium at {}", self.base));
        }
        let mut cause: &dyn std::error::Error = &e;
        while let Some(next) = cause.source() {
            cause = next;
        }
        let why = if e.is_timeout() { "it did not answer in time".to_owned() } else { cause.to_string() };
        anyhow!(
            "The Cactarium at {} isn't reachable ({why}). Check the network, or point cactup at another \
             Cactarium with the {URL_KNOB} knob or CACTARIUM_URL",
            self.base
        )
    }

    /// [`Client::send`] for a request that changes something on the
    /// Cactarium (`what`): interrupted in flight, it may have done so.
    fn send_change(&self, request: reqwest::blocking::RequestBuilder, what: &str) -> Res<Answer> {
        self.send(request).map_err(|e| match e.downcast_ref::<InFlight>() {
            Some(_) => anyhow!("Interrupted; {what} may have reached the Cactarium: check before trying again"),
            None => e,
        })
    }

    /// Send, then refuse a registry newer than this cactup speaks, and turn
    /// an error status into the message the registry wrote for it.
    fn send(&self, request: reqwest::blocking::RequestBuilder) -> Res<Answer> {
        Ok(self.send_found(request, &[])?.expect("no status counts as missing"))
    }

    /// [`Client::send`], but an answer with one of the `missing` statuses is
    /// `Err(Missing)`: the server has no such thing (a package, or a route).
    /// The request, its whole answer read, runs [`interruptibly`]: Ctrl-C
    /// stops it at once.
    fn send_found(
        &self,
        request: reqwest::blocking::RequestBuilder,
        missing: &[reqwest::StatusCode],
    ) -> Res<Result<Answer, Missing>> {
        let client = self.clone();
        let missing = missing.to_vec();
        interruptibly(move || client.exchange(request, &missing), self.stop)
    }

    fn exchange(
        &self,
        request: reqwest::blocking::RequestBuilder,
        missing: &[reqwest::StatusCode],
    ) -> Res<Result<Answer, Missing>> {
        let request = match self.timeout {
            Some(timeout) => request.timeout(timeout),
            None => request,
        };
        let mut response = request
            .header(API_GENERATION_HEADER, API_GENERATION.to_string())
            .send()
            .map_err(|e| self.unreachable(e))?;
        let generation = (response.headers().get(API_GENERATION_HEADER))
            .map(|g| g.to_str().ok().and_then(|g| g.parse::<u32>().ok()).unwrap_or(0));
        if let Some(generation) = generation
            && generation > API_GENERATION
        {
            bail!(
                "The Cactarium at {} speaks API generation {generation}, newer than this cactup's \
                 {API_GENERATION}; run `cactup update`",
                self.base
            );
        }
        let status = response.status();
        if status.is_success() {
            let url = response.url().to_string();
            let headers = response.headers().clone();
            // Through `Read`: the client's wait bounds each stall (and the
            // per-request timeout, when set, the whole answer).
            let mut body = Vec::new();
            std::io::Read::read_to_end(&mut response, &mut body)
                .map_err(|e| self.unreachable_read(e, &url))?;
            return Ok(Ok(Answer { headers, body, url }));
        }
        if missing.contains(&status) {
            return Ok(Err(Missing { status, generation: generation.is_some() }));
        }
        let text = response.text().unwrap_or_default();
        // A missing or dead token: say how to get a working one.
        let login = if status == reqwest::StatusCode::UNAUTHORIZED {
            format!(" (run `cactup login` with a token from {}/settings)", self.base)
        } else {
            String::new()
        };
        let message = match serde_json::from_str::<ErrorBody>(&text) {
            Ok(body) => format!("{}{login}", body.error.message),
            Err(_) => format!("The Cactarium at {} answered {status}", self.base),
        };
        Err(anyhow::Error::new(Refused { status: status.as_u16(), message }))
    }

    /// A body that stopped arriving: said as [`Client::unreachable`] says a
    /// server that did not answer in time.
    fn unreachable_read(&self, e: std::io::Error, url: &str) -> anyhow::Error {
        if e.kind() == std::io::ErrorKind::TimedOut || e.to_string().contains("timed out") {
            return anyhow!(
                "The Cactarium at {} isn't reachable (it did not answer in time). Check the network, or point \
                 cactup at another Cactarium with the {URL_KNOB} knob or CACTARIUM_URL",
                self.base
            );
        }
        anyhow::Error::new(e).context(format!("Failed to read the answer from {url}"))
    }
}

/// The statuses a server without a route answers: 404, and 405 when another
/// route of that path takes another method.
const MISSING_ROUTE: &[reqwest::StatusCode] = &[reqwest::StatusCode::NOT_FOUND, reqwest::StatusCode::METHOD_NOT_ALLOWED];

/// An answer the server had nothing for.
#[derive(Debug)]
struct Missing {
    status: reqwest::StatusCode,
    /// It said which API generation it speaks: a Cactarium.
    generation: bool,
}

/// An error status the registry answered, with its message.
#[derive(Debug)]
pub struct Refused {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refused {}

impl Refused {
    /// Not answered now, rather than answered: rate-limited (429), not
    /// served to this caller (403), or the server failing (5xx). A read
    /// refused so leaves that one package unread; it never fails a command
    /// on its own.
    pub fn is_unanswered(&self) -> bool {
        self.status == 429 || self.status == 403 || self.status >= 500
    }
}

/// A successful answer, read whole.
#[derive(Debug)]
struct Answer {
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
    url: String,
}

impl Answer {
    fn headers(&self) -> &reqwest::header::HeaderMap {
        &self.headers
    }

    fn bytes(self) -> Res<Vec<u8>> {
        Ok(self.body)
    }

    fn text(self) -> Res<String> {
        let url = self.url.clone();
        String::from_utf8(self.body).with_context(|| format!("The Cactarium's answer from {url} is not text"))
    }

    fn json<T: DeserializeOwned>(self) -> Res<T> {
        let url = self.url.clone();
        let text = self.text()?;
        serde_json::from_str(&text).with_context(|| format!("The Cactarium's answer from {url} is not what cactup expects"))
    }
}

/// A Cactarium older than this cactup: it has no `endpoint`, so it cannot
/// say which package a repo is, or what provides a requirement.
#[derive(Debug)]
pub struct OldCactarium {
    pub base: String,
    pub endpoint: &'static str,
}

impl std::fmt::Display for OldCactarium {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The Cactarium at {} is older than this cactup (it has no {}), so cactup cannot resolve dependencies \
             with it",
            self.base, self.endpoint
        )
    }
}

impl std::error::Error for OldCactarium {}

/// The two kinds of registry entry an installation records a version of.
#[derive(Clone, Copy, Debug)]
pub enum EntryKind {
    Package,
    Stack,
}

/// Say that `slug` is yanked, when `version` says so. A range never chooses
/// a yanked version, so one here was asked for exactly: it is used, as asked,
/// never refused — yanked means "not by default", not "gone".
pub fn warn_if_yanked(slug: &str, version: &VersionInfo) {
    if version.yanked {
        let why = version.yank_reason.as_deref().filter(|r| !r.is_empty()).map(|r| format!(": {r}")).unwrap_or_default();
        println!("{}", format!("{slug} is yanked{why}. Using it because you asked for exactly that version.").yellow());
    }
}

/// One stack response: the release, whether the server marked it unpinned
/// (`@head`/`@master`) and its stack untrusted (a tip nobody reviews), and
/// where to report problems with it (its declared contact, else its repo).
pub struct StackResponse {
    pub stack: FrozenStack,
    pub unpinned: bool,
    pub untrusted: bool,
    pub contact: Option<String>,
}

/// A stack release for `req` (`owner/stack[@range|version|head|master]`):
/// a range resolved once to the exact release, and that release's thornlist
/// with every repo it pins pinned by `!REPO_COMMIT`.
pub struct StackRelease {
    /// The exact slug: `owner/stack@1.2.0`, or the `@head`/`@master` form.
    pub slug: String,
    /// The thornlist, pinned.
    pub text: String,
    /// The registry marked it unpinned (`@head`/`@master`).
    pub unpinned: bool,
    /// Where to report problems with the stack.
    pub contact: Option<String>,
}

/// See [`StackRelease`]. A branch tip of a stack the Cactarium does not trust
/// is code nobody has reviewed: it is said loudly, and taken only on a yes
/// typed at the terminal, or with `allow_untrusted`
/// (`--risky-allow-untrusted-artifacts`), which still prints the warning.
pub fn stack_release_text(db: &Database, req: &str, allow_untrusted: bool) -> Res<StackRelease> {
    stack_release_from(&Client::new(db), req, allow_untrusted)
}

/// [`stack_release_text`], from the registry `client` talks to.
fn stack_release_from(client: &Client, req: &str, allow_untrusted: bool) -> Res<StackRelease> {
    use cactup_ecosystem_common::slug::Slug;
    let slug: Slug = req.parse().map_err(|e| anyhow!("\"{req}\" is not a stack (owner/name[@version]): {e}"))?;
    let version = match slug.tip() {
        Some(_) => slug.at.clone().expect("a tip is an @ form"),
        None => {
            let resolved = client.resolve(req, "stack")?;
            if resolved.kind != "stack" {
                bail!("{} is a {}, not a stack", resolved.slug, resolved.kind);
            }
            warn_if_yanked(&resolved.slug, &resolved.version);
            resolved.version.version
        }
    };
    let StackResponse { stack, unpinned, untrusted, contact } =
        client.stack_release(slug.owner.as_str(), slug.name.as_str(), &version)?;
    if untrusted {
        confirm_untrusted(&stack.stack.to_string(), "stack", Unreviewed::Tip, allow_untrusted)?;
    }
    let pin_failed = || format!("Failed to pin stack {}", stack.stack);
    let (pins, unchecked) = crate::thornlist::recorded_pins(&stack.thornlist.text, &stack.repos).with_context(pin_failed)?;
    if !unchecked.is_empty() {
        println!(
            "{}",
            format!(
                "Downloaded without checking their recorded hash, which names no single file: {}",
                unchecked.join(", ")
            )
            .yellow()
        );
    }
    let text = crate::thornlist::pin_text(&stack.thornlist.text, &pins).with_context(pin_failed)?;
    Ok(StackRelease { slug: stack.stack.to_string(), text, unpinned, contact })
}

/// What an untrusted, unreviewed source is: a branch tip, or a commit no
/// published version names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unreviewed {
    Tip,
    Commit,
}

impl Unreviewed {
    fn noun(self) -> &'static str {
        match self {
            Unreviewed::Tip => "branch tip",
            Unreviewed::Commit => "commit",
        }
    }
}

/// Whether there is a terminal to ask a question on.
pub(crate) fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// The warning for an untrusted stack's or package's unreviewed source,
/// then the yes it needs: typed at a terminal, or given up front by the
/// flag. Without either there is nobody to ask, so it is refused.
pub(crate) fn confirm_untrusted(slug: &str, kind: &str, what: Unreviewed, allow: bool) -> Res<()> {
    confirm_untrusted_on(slug, kind, what, allow, interactive())
}

/// [`confirm_untrusted`], told whether there is a terminal to ask on.
pub(crate) fn confirm_untrusted_on(slug: &str, kind: &str, what: Unreviewed, allow: bool, interactive: bool) -> Res<()> {
    let why = match what {
        Unreviewed::Tip => format!(
            "{slug} is a branch tip of a {kind} the Cactarium does not trust. Branch tips of untrusted {kind}s \
             are never reviewed"
        ),
        Unreviewed::Commit => format!(
            "{slug} is a commit of a {kind} the Cactarium does not trust, and no published version names it, \
             so nobody has reviewed it"
        ),
    };
    eprintln!("{}", format!("WARNING: {why}: installing it may fetch and build arbitrary code.").red().bold());
    if allow {
        eprintln!("{}", "Going on anyway, as --risky-allow-untrusted-artifacts says.".yellow());
        return Ok(());
    }
    if !interactive {
        bail!(
            "Refusing the untrusted {} {slug} without a terminal to confirm it on; use a release \
             (owner/name@X.Y.Z or a range), or pass --risky-allow-untrusted-artifacts",
            what.noun()
        );
    }
    use std::io::Write;
    eprint!("Type yes to fetch it anyway: ");
    std::io::stderr().flush()?;
    let answer = crate::commands::read_line_interruptibly()?.unwrap_or_default();
    if answer.trim() != "yes" {
        bail!("Not fetching {slug}: use a release instead (owner/name@X.Y.Z or a range)");
    }
    Ok(())
}

/// A downloaded `.sim` is stamped as `want` (`owner/name/sim@X.Y.Z`): what
/// was asked for, and not some other sim of the entry.
fn check_stamp(bytes: &[u8], want: &str) -> Res<()> {
    let text = std::str::from_utf8(bytes).map_err(|_| anyhow!("The Cactarium's file for {want} is not text"))?;
    let file = cactup_ecosystem_common::sim::SimFile::from_toml(text)
        .map_err(|e| anyhow!("The Cactarium's file for {want}: {e}"))?;
    match file.sim.cactarium.map(|s| s.to_string()) {
        Some(stamp) if stamp == want => Ok(()),
        Some(stamp) => bail!("Asked the Cactarium for {want}, and it sent the file of {stamp}"),
        None => bail!("Asked the Cactarium for {want}, and it sent a file that does not say which sim it is"),
    }
}

/// A stack release's JSON, its `format` checked before the typed parse
/// (`FrozenStack` refuses keys it does not know, so a release from a newer
/// server would fail on whichever of them came first, with a message that
/// never says to upgrade).
fn stack_json(text: String, url: &str) -> Res<FrozenStack> {
    let unexpected = || format!("The Cactarium's answer from {url} is not what cactup expects");
    let value: serde_json::Value = serde_json::from_str(&text).with_context(unexpected)?;
    if let Some(found) = value.get("format").and_then(serde_json::Value::as_u64)
        && found > u64::from(STACK_FORMAT)
    {
        bail!(
            "The Cactarium's stack release from {url} is format {found}, but this cactup, {}, reads up to format \
             {STACK_FORMAT}; upgrade cactup (`cactup update`)",
            crate::build_info::LONG_VERSION
        );
    }
    serde_json::from_value(value).with_context(unexpected)
}

/// `req` as a query-string value: everything outside the unreserved set
/// percent-encoded (a range's `^`, `:` and `@` included).
fn query_escape(req: &str) -> String {
    req.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// `~/.cactup/credentials`: one API token per registry, readable only by
/// its owner. Tokens are never knobs — knob values are frozen into every
/// restart's metadata, and a token there would travel with every simulation.
pub fn credentials_path() -> PathBuf {
    crate::CACTUP_ROOT.join("credentials")
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Credentials {
    /// Registry base URL → token.
    #[serde(default)]
    tokens: std::collections::BTreeMap<String, String>,
}

/// The credentials file's format. Login binaries only. Keys this binary does
/// not know (a newer one's) are put back on write, like any record's; the
/// token table is a map, so a token another binary stored for a registry this
/// one never heard of stays.
pub static CREDENTIALS: Format<Toml> = Format {
    name: "credentials",
    key: "schema",
    current: 1,
    upgrades: &[],
    downgrades: &[],
    down_floor: 1,
    frozen_read: false,
    legacy: None,
    replace_whole: &[],
};

/// The file's contents, empty when there is no file. A file that will not
/// parse is reported without the parser's detail: it quotes the offending
/// line, and the line may be a token, which must never reach a message.
fn read_credentials(path: &Path) -> Res<Credentials> {
    match format::read(path, &CREDENTIALS) {
        Ok(found) => Ok(found.unwrap_or_default()),
        Err(e) if e.downcast_ref::<format::Newer>().is_some() => Err(e),
        // The file could not be read at all: the error says why and holds no content.
        Err(e) if e.root_cause().is::<std::io::Error>() => Err(e),
        Err(_) => Err(anyhow!(
            "Failed to parse '{}': it is not a valid credentials file (its contents are not shown: they hold tokens)",
            path.display()
        )),
    }
}

/// The token for `base`: `CACTARIUM_TOKEN`, else the credentials file.
pub fn token_for(base: &str) -> Res<String> {
    if let Ok(token) = std::env::var("CACTARIUM_TOKEN")
        && !token.is_empty()
    {
        return Ok(token);
    }
    read_credentials(&credentials_path())?
        .tokens
        .remove(base)
        .ok_or_else(|| anyhow!("No token for the Cactarium at {base}; run `cactup login` first"))
}

/// Store `token` for `base` in `path`, written whole under a temporary name
/// created owner-only, then renamed into place: [`format::write_private`]
/// keeps the file 0600 whatever its umask or earlier mode, and puts back what
/// a newer cactup stored in it.
pub fn store_token(path: &Path, base: &str, token: &str) -> Res<()> {
    let mut credentials = read_credentials(path)?;
    credentials.tokens.insert(base.to_owned(), token.to_owned());
    format::write_private(path, &credentials, &CREDENTIALS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::download::test_server::serve_with;

    #[test]
    fn resolve_sends_the_request_escaped_and_reads_the_answer() {
        let answer = r#"{"slug":"max-morris/carpetx@mixed-precision:1.4.0","kind":"package","url":"https://x/carpetx.git",
            "version":"1.4.0","track":"mixed-precision","tag":"mixed-precision/v1.4.0","commit":"3f2c","tree":null,
            "yanked":false,"yank-reason":null,"state":"published","curated":false,"published":0}"#;
        let (base, heads) = serve_with(1, move |_| ("200 OK", "api-generation: 1\r\n".into(), answer.as_bytes().to_vec()));
        let resolved = Client::at(&base).resolve("max-morris/carpetx@mixed-precision:^1.2", "package").unwrap();
        assert_eq!(resolved.version.tag.as_deref(), Some("mixed-precision/v1.4.0"));
        let head = &heads.join().unwrap()[0];
        assert!(head.starts_with("GET /api/v1/resolve?req=max-morris/carpetx%40mixed-precision%3A%5E1.2&kind=package "), "{head}");
        assert!(head.to_ascii_lowercase().contains("api-generation: 1"), "{head}");
    }

    #[test]
    fn errors_carry_the_registrys_message_and_newer_generations_are_refused() {
        let body = r#"{"error":{"code":"not-found","message":"No sim mmorris/nope"}}"#;
        let (base, _) = serve_with(1, move |_| ("404 Not Found", String::new(), body.as_bytes().to_vec()));
        let err = format!("{:#}", Client::at(&base).resolve("mmorris/nope", "sim").unwrap_err());
        assert_eq!(err, "No sim mmorris/nope");

        let (base, _) = serve_with(1, |_| ("200 OK", "api-generation: 99\r\n".into(), b"{}".to_vec()));
        let err = format!("{:#}", Client::at(&base).resolve("a/b", "package").unwrap_err());
        assert!(err.contains("generation 99") && err.contains("cactup update"), "{err}");
    }

    #[test]
    fn the_url_knob_takes_a_registry_address() {
        assert_eq!(validate_url("https://cactarium.example.org/").unwrap(), "https://cactarium.example.org");
        assert_eq!(validate_url(" http://localhost:8000 ").unwrap(), "http://localhost:8000");
        for bad in ["cactarium.example.org", "ftp://x.org", "https://x.org/?a=1", "https://x.org/#f", "https:// x.org", ""] {
            assert!(validate_url(bad).is_err(), "{bad}");
        }
        assert!(crate::database::knob_spec(URL_KNOB).is_some(), "a built-in knob");
    }

    #[test]
    fn an_unreachable_registry_is_one_line_naming_it() {
        // A port nothing listens on: bound, then let go.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let err = format!("{:#}", Client::at(&base).resolve("a/b", "package").unwrap_err());
        assert!(err.starts_with(&format!("The Cactarium at {base} isn't reachable (")), "{err}");
        assert!(err.contains("cactarium-url") && !err.contains("error sending request"), "{err}");
    }

    #[test]
    fn a_sim_download_is_checked_against_the_registrys_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let good = sha256_hex(b"format = 1\n");
        let (base, _) = serve_with(2, move |path| {
            let hash = if path.contains("/1.0.0/") { good.clone() } else { "0".repeat(64) };
            ("200 OK", format!("x-sha256: {hash}\r\n"), b"format = 1\n".to_vec())
        });
        let client = Client::at(&base);
        client.download_sim("o", "n", "1.0.0", None, &tmp.path().join("ok.sim")).unwrap();
        assert_eq!(std::fs::read_to_string(tmp.path().join("ok.sim")).unwrap(), "format = 1\n");
        let err = format!("{:#}", client.download_sim("o", "n", "2.0.0", None, &tmp.path().join("bad.sim")).unwrap_err());
        assert!(err.contains("SHA-256"), "{err}");
        assert!(!tmp.path().join("bad.sim").exists());
    }

    /// One sim of a multi-sim entry is asked for by `?sim=`, and its own hash
    /// is checked, and so is its stamp: a file of another sim, or one with no
    /// stamp, is refused, and nothing is written. An entry that is one sim is
    /// asked for without `?sim=`.
    #[test]
    fn one_sim_of_several_is_asked_for_by_name() {
        use crate::commands::import::tests::{stamped, unstamped};
        let tmp = tempfile::tempdir().unwrap();
        let (base, heads) = serve_with(5, move |path| {
            let (body, hash) = match path.rsplit_once("?sim=").map(|(_, sim)| sim) {
                Some("qc0-z4c") => (stamped("et/sims/qc0-z4c@1.0.0"), None),
                Some("mag-tov") => (stamped("et/sims/mag-tov@1.0.0"), Some("0".repeat(64))),
                Some("other") => (stamped("et/sims/qc0-z4c@1.0.0"), None),
                Some(_) => (unstamped(), None),
                None => (stamped("et/one@1.0.0"), None),
            };
            let hash = hash.unwrap_or_else(|| sha256_hex(body.as_bytes()));
            ("200 OK", format!("x-sha256: {hash}\r\n"), body.into_bytes())
        });
        let client = Client::at(&base);
        let at = |file: &str| tmp.path().join(file);
        client.download_sim("et", "sims", "1.0.0", Some("qc0-z4c"), &at("ok.sim")).unwrap();
        let fails = |sim: &str, file: &str| {
            let err = format!("{:#}", client.download_sim("et", "sims", "1.0.0", Some(sim), &at(file)).unwrap_err());
            assert!(!at(file).exists(), "{file} was written");
            err
        };
        let err = fails("mag-tov", "hash.sim");
        assert!(err.contains("et/sims/mag-tov@1.0.0") && err.contains("SHA-256"), "{err}");
        let err = fails("other", "other.sim");
        assert!(err.contains("et/sims/other@1.0.0") && err.contains("sent the file of et/sims/qc0-z4c@1.0.0"), "{err}");
        let err = fails("bare", "bare.sim");
        assert!(err.contains("does not say which sim it is"), "{err}");
        client.download_sim("et", "one", "1.0.0", None, &at("one.sim")).unwrap();
        let heads = heads.join().unwrap();
        assert!(heads[0].starts_with("GET /api/v1/sims/et/sims/1.0.0/download?sim=qc0-z4c "), "{}", heads[0]);
        assert!(heads[1].starts_with("GET /api/v1/sims/et/sims/1.0.0/download?sim=mag-tov "), "{}", heads[1]);
        assert!(heads[4].starts_with("GET /api/v1/sims/et/one/1.0.0/download "), "{}", heads[4]);
    }

    /// A stack install asks `/resolve` for a stack (`kind=stack`), so a
    /// Cactarium that knows the parameter can say when the entry is another
    /// kind; an answer of another kind is refused here too.
    #[test]
    fn a_stack_is_resolved_as_a_stack() {
        let release = r#"{"format":1,"stack":"et/et-stack@1.0.0","root":"Cactus","repo":[],
            "thornlist":{"filename":"et.th","text":"!CRL_VERSION = 1.0\n"}}"#;
        let answer = |kind: &str| {
            format!(
                r#"{{"slug":"et/et-stack@1.0.0","kind":"{kind}","url":null,"contact":null,"version":"1.0.0","track":null,
                "tag":"v1.0.0","commit":null,"tree":null,"yanked":false,"yank-reason":null,"state":"ok","curated":false,"published":0}}"#
            )
        };
        let (base, heads) = serve_with(3, move |path| {
            let body = match path {
                p if p.contains("/resolve?req=et/et-stack%40") => answer("stack"),
                p if p.contains("/resolve?") => answer("package"),
                _ => release.to_owned(),
            };
            ("200 OK", "api-generation: 1\r\nx-cactarium-untrusted: false\r\n".into(), body.into_bytes())
        });
        let client = Client::at(&base);
        let stack = stack_release_from(&client, "et/et-stack@1", false).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(stack.slug, "et/et-stack@1.0.0");
        let err = format!("{:#}", stack_release_from(&client, "et/carpetx", false).err().unwrap());
        assert!(err.contains("is a package, not a stack"), "{err}");
        let heads = heads.join().unwrap();
        assert!(heads[0].starts_with("GET /api/v1/resolve?req=et/et-stack%401&kind=stack "), "{}", heads[0]);
        assert!(heads[2].starts_with("GET /api/v1/resolve?req=et/carpetx&kind=stack "), "{}", heads[2]);
    }

    /// A tip asked for, answered with neither header, is untrusted: the
    /// check fails closed whatever the registry leaves out.
    #[test]
    fn a_tip_without_headers_is_untrusted() {
        let body = r#"{"format":1,"stack":"et/et-stack@head","root":"Cactus","repo":[],
            "thornlist":{"filename":"et.th","text":"!CRL_VERSION = 1.0\n"}}"#;
        let (base, _) = serve_with(1, move |_| ("200 OK", String::new(), body.as_bytes().to_vec()));
        assert!(Client::at(&base).stack_release("et", "et-stack", "head").unwrap().untrusted);
    }

    /// A stack release from a newer format says to upgrade, before a key
    /// the typed struct does not know could make it fail some other way.
    #[test]
    fn a_newer_stack_format_says_to_upgrade() {
        let body = |format: u32| {
            format!(
                r#"{{"format":{format},"stack":"et/et-stack@1.0.0","root":"Cactus","repo":[],"future-key":true,
                "thornlist":{{"filename":"et.th","text":"!CRL_VERSION = 1.0\n"}}}}"#
            )
        };
        let (base, _) = serve_with(1, move |_| ("200 OK", String::new(), body(STACK_FORMAT + 1).into_bytes()));
        let Err(err) = Client::at(&base).stack_release("et", "et-stack", "1.0.0") else { panic!("a newer format was read") };
        let err = format!("{err:#}");
        assert!(err.contains("format 2") && err.contains("upgrade cactup") && !err.contains("future-key"), "{err}");

        // The format a client reads is read, and a broken body is still not what it expects.
        let (base, _) = serve_with(1, move |_| ("200 OK", String::new(), body(STACK_FORMAT).into_bytes()));
        let err = format!("{:#}", Client::at(&base).stack_release("et", "et-stack", "1.0.0").err().unwrap());
        assert!(err.contains("not what cactup expects"), "the unknown key is still refused: {err}");
    }

    /// A tip response says whether its stack is trusted; an untrusted one is
    /// taken with the flag, and refused with nobody at a terminal to ask.
    #[test]
    fn an_untrusted_tip_needs_a_yes() {
        let body = r#"{"format":1,"stack":"et/et-stack@head","root":"Cactus","repo":[],
            "thornlist":{"filename":"et.th","text":"!CRL_VERSION = 1.0\n"}}"#;
        let (base, _) = serve_with(2, move |path| {
            let untrusted = if path.contains("/et/") { "true" } else { "false" };
            ("200 OK", format!("x-cactarium-unpinned: true\r\nx-cactarium-untrusted: {untrusted}\r\n"), body.as_bytes().to_vec())
        });
        let client = Client::at(&base);
        let response = |owner: &str| client.stack_release(owner, "s", "head").unwrap_or_else(|e| panic!("{e:#}"));
        let tip = response("et");
        assert!(tip.unpinned && tip.untrusted);
        assert!(!response("max").untrusted);

        confirm_untrusted_on("et/et-stack@head", "stack", Unreviewed::Tip, true, false).unwrap();
        let err = format!("{:#}", confirm_untrusted_on("et/et-stack@head", "stack", Unreviewed::Tip, false, false).unwrap_err());
        assert!(err.contains("without a terminal") && err.contains("--risky-allow-untrusted-artifacts"), "{err}");
    }

    /// Whether an installed version was yanked since: from the package's
    /// version list or the stack's release list, with the reason; a version
    /// the registry lists unyanked, or no longer lists, is not.
    #[test]
    fn yanked_since_reads_the_version_lists() {
        let v = |version: &str, yanked: bool, reason: &str| {
            format!(
                r#"{{"version":"{version}","track":null,"tag":null,"commit":"c","tree":null,"yanked":{yanked},
                "yank-reason":{},"state":"ok","curated":false,"published":0}}"#,
                if reason.is_empty() { "null".to_owned() } else { format!("\"{reason}\"") }
            )
        };
        let packages = format!(
            r#"{{"slug":"et/carpetx","versions":[{{{},"url":"u","thorns":[],"claims":[]}},{{{},"url":"u","thorns":[],"claims":[]}}]}}"#,
            &v("1.1.0", true, "breaks Loop")[1..v("1.1.0", true, "breaks Loop").len() - 1],
            &v("1.0.0", false, "")[1..v("1.0.0", false, "").len() - 1]
        );
        let stacks = format!(r#"{{"slug":"et/et-stack","releases":[{},{}]}}"#, v("2026.5.0", true, ""), v("2026.5.1", false, ""));
        let (base, _) = serve_with(5, move |path| {
            let body = if path.contains("/packages/") { packages.clone() } else { stacks.clone() };
            ("200 OK", "api-generation: 1\r\n".into(), body.into_bytes())
        });
        let client = Client::at(&base);
        use EntryKind::*;
        assert_eq!(client.yanked_since(Package, "et", "carpetx", "1.1.0").unwrap(), Some("breaks Loop".into()));
        assert_eq!(client.yanked_since(Package, "et", "carpetx", "1.0.0").unwrap(), None);
        assert_eq!(client.yanked_since(Package, "et", "carpetx", "0.9.0").unwrap(), None, "no longer listed");
        assert_eq!(client.yanked_since(Stack, "et", "et-stack", "2026.5.0").unwrap(), Some(String::new()));
        assert_eq!(client.yanked_since(Stack, "et", "et-stack", "2026.5.1").unwrap(), None);
    }

    /// Every pin goes out in one POST (the endpoint is rate-limited per
    /// address), split only at the size limit, and the answers come back in
    /// the order asked; a pin the server would refuse is refused before
    /// anything is sent.
    #[test]
    fn a_lookup_batches_every_pin() {
        use crate::fetch::download::test_server::serve_with_body;
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let (base, heads) = serve_with_body(2, move |_, body| {
            let asked: LookupRequest = serde_json::from_slice(body).unwrap();
            seen.lock().unwrap().push(asked.pins.len());
            let pins: Vec<String> = (asked.pins.iter())
                .map(|p| {
                    format!(
                        r#"{{"url":"{}","commit":"{}","package":"et/x","version":null,"commit-known":null}}"#,
                        p.url, p.commit
                    )
                })
                .collect();
            ("200 OK", "api-generation: 1\r\n".into(), format!(r#"{{"pins":[{}]}}"#, pins.join(",")).into_bytes())
        });
        let pins: Vec<PinQuery> = (0..MAX_LOOKUP_PINS + 1)
            .map(|i| PinQuery { url: format!("https://x/r{i}.git"), commit: format!("{i:040x}") })
            .collect();
        let answers = Client::at(&base).lookup(&pins).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(answers.len(), pins.len());
        assert!(answers.iter().zip(&pins).all(|(a, p)| a.url == p.url && a.commit_known.is_none()));
        assert_eq!(*bodies.lock().unwrap(), [MAX_LOOKUP_PINS, 1]);
        let heads = heads.join().unwrap();
        assert!(heads.iter().all(|h| h.starts_with("POST /api/v1/lookup ")), "{heads:?}");

        // Nothing listens here: refused before sending.
        let bad = [PinQuery { url: "https://x/r.git".into(), commit: "abc".into() }];
        let err = format!("{:#}", Client::at("http://127.0.0.1:9").lookup(&bad).unwrap_err());
        assert!(err.contains("not a full lowercase commit id"), "{err}");
    }

    /// `/providers` is asked with the one parameter the requirement maps to;
    /// a package the Cactarium does not have reads as none, not an error.
    #[test]
    fn providers_and_missing_packages() {
        use cactup_ecosystem_common::ccl::Need;
        let (base, heads) = serve_with(2, |path| {
            if path.contains("/providers") {
                let body = r#"{"providers":[{"package":"et/carpet","version":"mp:2026.5.0","arrangement":"Carpet","thorn":"CarpetSlab"}]}"#;
                return ("200 OK", "api-generation: 1\r\n".into(), body.as_bytes().to_vec());
            }
            let body = r#"{"error":{"code":"not-found","message":"No package et/nope"}}"#;
            ("404 Not Found", "api-generation: 1\r\n".into(), body.as_bytes().to_vec())
        });
        let client = Client::at(&base);
        let found = client.providers(&Need::Implementation("Hyperslab".into())).unwrap();
        assert_eq!(found.providers[0].version, "mp:2026.5.0");
        assert!(client.package_versions_if_any("et", "nope").unwrap().is_none());
        let heads = heads.join().unwrap();
        assert!(heads[0].starts_with("GET /api/v1/providers?implements=Hyperslab "), "{}", heads[0]);
    }

    /// A Cactarium older than this cactup has no `/lookup` or `/providers`:
    /// a 404 (or 405) there means the route is missing, since neither
    /// endpoint answers 404 about what it is asked, and its API fallback
    /// still says which generation it speaks. Without that header the
    /// address is no Cactarium's. A 404 for a package is that package
    /// missing; a 405 there is no such thing, but an error.
    #[test]
    fn an_older_cactarium_is_told_apart_from_a_missing_package_and_a_wrong_url() {
        use cactup_ecosystem_common::ccl::Need;
        let pins = [PinQuery { url: "https://x/r.git".into(), commit: "a".repeat(40) }];
        let (base, _) = serve_with(4, |path| {
            let status = if path.contains("/lookup") { "405 Method Not Allowed" } else { "404 Not Found" };
            (status, "api-generation: 1\r\n".into(), br#"{"error":{"code":"not-found","message":"Not found"}}"#.to_vec())
        });
        let client = Client::at(&base);
        let err = client.lookup(&pins).unwrap_err();
        assert!(err.downcast_ref::<OldCactarium>().is_some_and(|o| o.endpoint == "/lookup"), "{err:#}");
        assert!(format!("{err:#}").contains("older than this cactup"), "{err:#}");
        let err = client.providers(&Need::Capability("HDF5".into())).unwrap_err();
        assert!(err.downcast_ref::<OldCactarium>().is_some_and(|o| o.endpoint == "/providers"), "{err:#}");
        assert!(client.package_versions_if_any("et", "nope").unwrap().is_none());
        assert!(client.package_at_if_any("et", "nope", &"b".repeat(40)).unwrap().is_none());

        // Some other web server: no API generation on its 404.
        let (base, _) = serve_with(2, |_| ("404 Not Found", String::new(), b"<html>nope</html>".to_vec()));
        let client = Client::at(&base);
        let err = format!("{:#}", client.lookup(&pins).unwrap_err());
        assert!(err.contains("does not look like a Cactarium") && err.contains("cactarium-url"), "{err}");
        let err = client.providers(&Need::Capability("HDF5".into())).unwrap_err();
        assert!(err.downcast_ref::<OldCactarium>().is_none(), "{err:#}");

        // A 405 for a package is no missing package.
        let (base, _) = serve_with(2, |_| ("405 Method Not Allowed", "api-generation: 1\r\n".into(), Vec::new()));
        let client = Client::at(&base);
        assert!(client.package_versions_if_any("et", "x").is_err());
        assert!(client.package_at_if_any("et", "x", &"b".repeat(40)).is_err());
    }

    /// Every request gives way to Ctrl-C within a poll or two, the body
    /// included: a server that never answers, and one that answers its head
    /// and then trickles its body, are both left at once.
    #[test]
    fn a_request_in_flight_gives_way_to_ctrl_c() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicBool, Ordering};
        static STOP: AtomicBool = AtomicBool::new(false);
        for trickle in [false, true] {
            STOP.store(false, Ordering::Relaxed);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut head = [0u8; 1024];
                let _ = stream.read(&mut head);
                if trickle {
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\napi-generation: 1\r\nContent-Length: 400\r\n\r\n");
                    for _ in 0..400 {
                        if stream.write_all(b" ").and_then(|()| stream.flush()).is_err() {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                } else {
                    std::thread::sleep(std::time::Duration::from_secs(40));
                }
            });
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(200));
                STOP.store(true, Ordering::Relaxed);
            });
            let started = std::time::Instant::now();
            let err = Client::at(&base).stopped_by(&STOP).package_versions("et", "x").unwrap_err();
            assert_eq!(err.to_string(), "Interrupted", "trickle {trickle}");
            assert!(started.elapsed() < std::time::Duration::from_secs(1), "trickle {trickle}: {:?}", started.elapsed());
        }
        // An answer comes back as it is.
        assert_eq!(interruptibly(|| Ok(7), &AtomicBool::new(false)).unwrap(), 7);
    }

    /// After Ctrl-C nothing goes out; a change interrupted in flight says it
    /// may have reached the Cactarium.
    #[test]
    fn a_change_interrupted_in_flight_says_it_may_have_reached_the_cactarium() {
        use std::io::Read;
        use std::sync::atomic::{AtomicBool, Ordering};
        static STOPPED: AtomicBool = AtomicBool::new(true);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let client = Client::at(&base).stopped_by(&STOPPED);
        let err = client.send_change(http_post(&client), "the refresh").unwrap_err();
        assert_eq!(err.to_string(), "Interrupted");
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(listener.accept().is_err(), "a request went out after Ctrl-C");

        static STOP: AtomicBool = AtomicBool::new(false);
        listener.set_nonblocking(false).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = [0u8; 1024];
            let _ = stream.read(&mut head);
            STOP.store(true, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_secs(5));
        });
        let client = Client::at(&base).stopped_by(&STOP);
        let err = format!("{:#}", client.send_change(http_post(&client), "the refresh").unwrap_err());
        assert!(err.contains("the refresh may have reached the Cactarium") && err.contains("check before"), "{err}");
    }

    fn http_post(client: &Client) -> reqwest::blocking::RequestBuilder {
        client.http().post(client.url("/packages/et/x/refresh"))
    }

    /// Rate limits, refusals to serve and failing servers are answers not
    /// given now; anything else is an answer.
    #[test]
    fn unanswered_statuses() {
        let refused = |status| Refused { status, message: String::new() }.is_unanswered();
        assert!(refused(429) && refused(403) && refused(502) && refused(500));
        assert!(!refused(404) && !refused(400) && !refused(401));
    }

    /// A client with a whole-request limit gives up on an answer whose body
    /// trickles in past it, however short each stall.
    #[test]
    fn a_whole_request_limit_covers_the_body() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = [0u8; 1024];
            let _ = stream.read(&mut head);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\napi-generation: 1\r\nContent-Length: 400\r\n\r\n");
            for _ in 0..400 {
                if stream.write_all(b" ").and_then(|()| stream.flush()).is_err() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
        let started = std::time::Instant::now();
        let client = Client::at(&base).with_timeout(std::time::Duration::from_millis(1500));
        let err = client.package_versions("et", "x").unwrap_err();
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(4), "{elapsed:?}: {err:#}");
    }

    #[test]
    fn tokens_are_stored_per_registry_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials");
        store_token(&path, "https://a", "cactarium_one").unwrap();
        store_token(&path, "https://b", "cactarium_two").unwrap();
        let credentials = read_credentials(&path).unwrap();
        assert_eq!(credentials.tokens["https://a"], "cactarium_one");
        assert_eq!(credentials.tokens["https://b"], "cactarium_two");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    fn exhaustive_credentials() -> Credentials {
        let mut tokens = std::collections::BTreeMap::new();
        tokens.insert("https://cactarium.example.org".to_owned(), "dummy-token-one".to_owned());
        tokens.insert("https://mirror.example.org".to_owned(), "dummy-token-two".to_owned());
        Credentials { tokens }
    }

    #[test]
    fn credentials_format_generic_tests() {
        use crate::format::testing::*;
        let sample = exhaustive_credentials();
        descriptor_is_consistent(&CREDENTIALS);
        let fixtures = fixture_dir("credentials");
        shape_is_pinned(&CREDENTIALS, &fixtures, &sample);
        every_fixture_loads::<Credentials, _>(&CREDENTIALS, &fixtures);
        newer_is_refused::<Credentials, _>(&CREDENTIALS, "credentials", "tokens = 7\n", &sample);
        unknown_keys_survive(
            &CREDENTIALS,
            "credentials",
            &sample,
            // `tokens` maps to strings, so a nested unknown key has no shape of its own.
            &[],
            |c: &mut Credentials| drop(c.tokens.remove("https://mirror.example.org")),
            &["tokens", "https://mirror.example.org"],
        );
    }

    /// The file stays owner-only through every write, including over a file
    /// that was wider, and what a newer cactup put in it (and the schema key)
    /// is there afterward.
    #[test]
    fn credentials_stay_owner_only_and_keep_unknown_keys() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials");
        // One that predates the schema key, readable by everyone.
        std::fs::write(
            &path,
            "future = 1\n\n[tokens]\n\"https://a\" = \"cactarium_one\"\n\n[tokens-future]\nx = 2\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        store_token(&path, "https://b", "cactarium_two").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("schema = 1\n"), "{text}");
        let doc: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(doc["future"].as_integer(), Some(1), "{text}");
        assert_eq!(doc["tokens-future"]["x"].as_integer(), Some(2), "{text}");
        assert_eq!(doc["tokens"]["https://a"].as_str(), Some("cactarium_one"));
        assert_eq!(doc["tokens"]["https://b"].as_str(), Some("cactarium_two"));
        let left: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temp file is left behind");
    }

    /// A broken or newer file's error never carries a token.
    #[test]
    fn credentials_errors_do_not_show_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials");
        // Not a table of tokens: the parser's message would quote the value.
        std::fs::write(&path, "tokens = \"cactarium_secret\"\n").unwrap();
        let err = format!("{:#}", store_token(&path, "https://a", "cactarium_new").unwrap_err());
        assert!(err.contains("credentials") && !err.contains("cactarium_"), "{err}");
        std::fs::write(&path, "tokens = [ \"cactarium_secret\"\n").unwrap();
        let err = format!("{:#}", read_credentials(&path).unwrap_err());
        assert!(err.contains("credentials") && !err.contains("cactarium_"), "{err}");

        let newer = "schema = 2\n[tokens]\n\"https://a\" = \"cactarium_secret\"\n";
        std::fs::write(&path, newer).unwrap();
        let err = format!("{:#}", store_token(&path, "https://a", "cactarium_new").unwrap_err());
        assert!(err.contains("upgrade cactup") && !err.contains("cactarium_"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    }
}
