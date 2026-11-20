//! Whether a git remote answers a stranger: the anonymous probe every git
//! remote in a `.sim` must pass before it is published (spec §8.9). The
//! rules are the shared crate's (`cactup_ecosystem_common::probe`), so the
//! Cactarium decides exactly as cactup does: [`probe::start`] refuses a URL
//! by its form or names the candidate to publish (an https or http remote
//! itself, an ssh remote's https equivalent), and [`probe::next_step`],
//! given what each probe found ([`probe::Outcome`]), publishes the
//! candidate, asks once more at a redirect to the same repo (GitLab's
//! `.git` form), or refuses. This module does only the I/O.
//!
//! The request is a bare GET of [`probe::probe_url`] (the smart-HTTP ref
//! advertisement, what `git ls-remote` asks first), made with reqwest
//! directly rather than gix: gix consults the user's credential helpers
//! (even with prompts off), `.netrc` and `http.extraHeader`, and a private
//! repo the user happens to have access to would look public and be
//! published to people who cannot fetch it. This client sends nothing that
//! could identify anyone: no `Authorization` header, no cookies (it keeps
//! none), no `.netrc` (reqwest reads none), no `Referer`, and it follows no
//! redirect. Its host is resolved once, here, and every address must be
//! public ([`probe::is_public_address`]): a remote that resolves to a
//! private address is not reachable for others. The connection is pinned
//! to the addresses checked, so DNS cannot swap in another between the
//! check and the request. (A proxy from the environment is used as for any
//! download; the proxy then connects on its own, and the pin cannot hold.)

use super::git::AskWindow;
use super::watch::{abandonable, Bounds, Stop};
use crate::Res;
use anyhow::bail;
use cactup_ecosystem_common::probe::{self, Outcome, Step};
use cactup_ecosystem_common::url::{private_address_refusal, unanswered_refusal, unreachable_refusal};
use std::time::{Duration, Instant};

/// The longest a probe waits to connect (within what is left of its window).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What the probe decided for a git remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Publish this URL for it: the remote as written, or an ssh remote's
    /// https equivalent.
    Publish(String),
    /// Refused, with the message to show (URLs redacted), what the last
    /// probe saw appended.
    Refuse(String),
}

/// Check the git remote `url` (whose [`probe::start`] named `candidate`)
/// by the crate's steps, each probe within `window` and Ctrl-C: a request
/// runs on a thread of its own ([`super::watch`]), and a Ctrl-C fails the
/// check at once ("Interrupted"), the only error. At most two probes: the
/// candidate, and once more at a redirect to the same repo.
pub fn check(url: &str, candidate: &str, window: AskWindow) -> Res<Verdict> {
    let mut asked = candidate.to_owned();
    for round in 0..2 {
        // Nothing answered, or nothing was asked: not the repo's doing, so
        // not the "doesn't answer" wording, which blames the repo.
        let (outcome, seen) = match ask(&asked, window)? {
            Ok(response) => response,
            Err(Unchecked::Private(host)) => return Ok(Verdict::Refuse(private_address_refusal(url, &host))),
            Err(Unchecked::Unreachable(detail)) => return Ok(Verdict::Refuse(unreachable_refusal(url, &detail))),
        };
        let refused = |message: String| Verdict::Refuse(format!("{message} (asked without credentials, it {seen})"));
        match probe::next_step(url, candidate, &asked, &outcome) {
            Step::Publish(published) => return Ok(Verdict::Publish(published)),
            Step::Probe(target) if round == 0 => asked = target,
            Step::Probe(_) => return Ok(refused(unanswered_refusal(url, candidate))),
            Step::Refuse(message) => return Ok(refused(message)),
        }
    }
    unreachable!("the second round always decides")
}

/// [`check`] from the start: the remote's [`probe::start`], then its probes.
#[cfg(test)]
pub fn check_remote(url: &str, window: AskWindow) -> Res<Verdict> {
    match probe::start(url) {
        probe::Start::Refuse(message) => Ok(Verdict::Refuse(message)),
        probe::Start::Probe { candidate } => check(url, &candidate, window),
    }
}

/// Why a remote could not be checked at all: no response came, or it was
/// not asked. Said by the crate's `private_address_refusal` and
/// `unreachable_refusal`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Unchecked {
    /// Its host, as written, resolves to an address that is not public.
    Private(String),
    /// No HTTP response, and why, in a few words with no URL in them
    /// ("connection refused", "no answer within 30 s").
    Unreachable(String),
}

/// What went wrong with a request that got no response, in a few words
/// built from the error's kind, never its text: reqwest's quotes the URL.
fn failure(e: &reqwest::Error) -> String {
    use std::io::ErrorKind;
    if e.is_timeout() {
        return "connection timed out".to_owned();
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            match io.kind() {
                ErrorKind::ConnectionRefused => return "connection refused".to_owned(),
                ErrorKind::TimedOut => return "connection timed out".to_owned(),
                ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => return "connection reset".to_owned(),
                ErrorKind::NetworkUnreachable | ErrorKind::HostUnreachable => return "no route to the host".to_owned(),
                _ => {}
            }
        }
        let said = err.to_string().to_ascii_lowercase();
        if said.contains("dns error") || said.contains("failed to lookup") || said.contains("name or service not known") {
            return "dns lookup failed".to_owned();
        }
        if said.contains("certificate") || said.contains("tls") || said.contains("handshake") {
            return "TLS handshake failed".to_owned();
        }
        source = err.source();
    }
    if e.is_connect() { "connection failed" } else { "request failed" }.to_owned()
}

/// What a probe got back: what it found, and what it saw, as a clause
/// ("answered 401 Unauthorized", "redirected to …").
type Response = (Outcome, String);

/// One probe of `asked`, within what is left of `window`: its response, or
/// why it got none.
fn ask(asked: &str, window: AskWindow) -> Res<Result<Response, Unchecked>> {
    let left = window.until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Ok(Err(Unchecked::Unreachable("the time for asking remotes was up".to_owned())));
    }
    #[cfg(test)]
    PROBED.lock().expect("test record poisoned").push(asked.to_owned());
    let owned = asked.to_owned();
    // The request reports no progress: the silence is the whole probe. Its
    // own timeout is a little longer than the watch's, so a silence is
    // said as the window, and the thread ends soon after on its own.
    let bounds = Bounds { stall: left, total: Some(left), wind_down: Duration::ZERO };
    match abandonable(gix::progress::Discard, bounds, move |_, _| request(&owned, left + Duration::from_secs(1))) {
        Ok(found) => Ok(found),
        // Only building the client fails this way; its words are no
        // network's.
        Err(Stop::Failed(_)) => Ok(Err(Unchecked::Unreachable("the probe failed".to_owned()))),
        Err(Stop::Interrupted) => bail!("Interrupted"),
        Err(Stop::Stalled(_) | Stop::Overran(_)) => {
            let secs = (window.length.as_secs_f64().round() as u64).max(1);
            Ok(Err(Unchecked::Unreachable(format!("no answer within {secs} s"))))
        }
        Err(Stop::Vanished) => Ok(Err(Unchecked::Unreachable("the probe failed".to_owned()))),
    }
}

/// The request itself, on the probe's thread (so the resolution, too, is
/// bounded by the window and given up on at a Ctrl-C): the host resolved
/// and checked, then a bare GET of the advertisement, pinned to those
/// addresses, turned into an [`Outcome`] by the crate's rules.
fn request(asked: &str, timeout: Duration) -> Res<Result<Response, Unchecked>> {
    use reqwest::header;
    let target = probe::probe_url(asked);
    let parsed = match reqwest::Url::parse(&target) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(Err(Unchecked::Unreachable("its URL does not parse".to_owned()))),
    };
    let mut client = client(timeout);
    match public_addresses(&parsed) {
        Ok(Some((host, addrs))) => client = client.resolve_to_addrs(&host, &addrs),
        Ok(None) => {}
        Err(unchecked) => return Ok(Err(unchecked)),
    }
    // A test's host is served by a local stand-in; the check above was of
    // the host asked, through the test resolver.
    #[cfg(test)]
    let parsed = match routed(&target) {
        Some(local) => reqwest::Url::parse(&local)?,
        None => parsed,
    };
    let response = match client.build()?.get(parsed).send() {
        Ok(response) => response,
        Err(e) => return Ok(Err(Unchecked::Unreachable(failure(&e)))),
    };
    let status = response.status();
    let kind = response.headers().get(header::CONTENT_TYPE).and_then(|t| t.to_str().ok()).map(str::to_owned);
    if probe::is_answer(status.as_u16(), kind.as_deref()) {
        return Ok(Ok((Outcome::Answer, "answered".to_owned())));
    }
    if status.is_redirection() {
        let location = response.headers().get(header::LOCATION).and_then(|l| l.to_str().ok());
        // Resolved against the URL asked, not a test's local stand-in.
        let to = location.and_then(|l| probe::redirect_target(&probe::probe_url(asked), l));
        let seen = match &to {
            Some(to) => format!("redirected to {}", cactup_ecosystem_common::url::Redacted(to)),
            None => format!("answered {status} with no location"),
        };
        return Ok(Ok((Outcome::Redirect(to), seen)));
    }
    let seen = match (status == reqwest::StatusCode::OK, kind) {
        (false, _) => format!("answered {status}"),
        (true, None) => "answered, but not as a git server (no content type)".to_owned(),
        (true, Some(kind)) => format!("answered, but not as a git server ({kind})"),
    };
    Ok(Ok((Outcome::Other, seen)))
}

/// The probe's client: no redirect followed, no referer, a git user agent
/// (`git/cactup-<version>`), nothing kept between probes (a client of its
/// own each), and a connect timeout within `timeout`, the request's. Tests
/// ignore the environment's proxies, so they ask their local servers
/// wherever they run.
fn client(timeout: Duration) -> reqwest::blocking::ClientBuilder {
    let builder = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .user_agent(format!("{}cactup-{}", probe::USER_AGENT_PREFIX, env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .timeout(timeout);
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder
}

/// `url`'s host resolved, once, with every address checked public: the
/// domain and its addresses to pin the connection to; `None` for an IP
/// address (checked, and needing no pin) and for a host that does not
/// resolve here; else, when an address is not public, that address.
///
/// A host that does not resolve here (no answer, no such name) is not
/// refused for that alone: a login node that reaches the world only
/// through a proxy cannot resolve names itself, and the proxy may still
/// reach the remote. The check and the pin are skipped, and the probe
/// decides: through a proxy it may answer, and without one it fails to
/// connect, which is then the refusal, with its own words. Leniency is
/// safe on this side: the Cactarium vets the addresses of anything
/// published there itself.
fn public_addresses(url: &reqwest::Url) -> Result<Option<(String, Vec<std::net::SocketAddr>)>, Unchecked> {
    let port = url.port_or_known_default().unwrap_or(443);
    let Some(host) = url.host_str() else { return Err(Unchecked::Unreachable("it has no host".to_owned())) };
    let (domain, addrs): (Option<String>, Vec<std::net::SocketAddr>) =
        match host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>() {
            Ok(ip) => (None, vec![(ip, port).into()]),
            Err(_) => match resolve(host, port) {
                Ok(addrs) if !addrs.is_empty() => (Some(host.to_owned()), addrs),
                _ => return Ok(None),
            },
        };
    if addrs.iter().any(|a| !probe::is_public_address(a.ip())) {
        return Err(Unchecked::Private(host.to_owned()));
    }
    Ok(domain.map(|d| (d, addrs)))
}

/// `host`'s addresses, as the system resolver gives them; in tests, a
/// routed host ([`ROUTES`]) resolves to a public address (its stand-in is
/// local) and one in [`UNRESOLVED`] not at all.
fn resolve(host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    #[cfg(test)]
    {
        if UNRESOLVED.lock().expect("test hosts poisoned").contains(host) {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no such host (test)"));
        }
        if ROUTES.lock().expect("test routes poisoned").contains_key(host) {
            return Ok(vec![(std::net::Ipv4Addr::new(140, 82, 112, 3), port).into()]);
        }
    }
    Ok((host, port).to_socket_addrs()?.collect())
}

/// Hosts that do not resolve, as on a login node that resolves nothing
/// itself (tests only; see [`resolve`]).
#[cfg(test)]
pub static UNRESOLVED: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Every URL probed in this process, in order (tests only).
#[cfg(test)]
pub static PROBED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Where a test's https host is served instead: `https://<host>` → a local
/// `http://127.0.0.1:<port>` ([`testserver::serve`]), since a probe asks
/// the candidate's own scheme and host and an ssh URL's port does not
/// survive into its https equivalent. Tests use hosts of their own, so
/// they can run side by side. A routed host resolves to a public address
/// ([`resolve`]): the only way past the public-address check, and only in
/// tests.
#[cfg(test)]
pub static ROUTES: std::sync::Mutex<std::collections::BTreeMap<String, String>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

#[cfg(test)]
fn routed(target: &str) -> Option<String> {
    let routes = ROUTES.lock().expect("test routes poisoned");
    routes.iter().find_map(|(host, base)| {
        ["https", "http"].iter().find_map(|scheme| target.strip_prefix(&format!("{scheme}://{host}/"))).map(|rest| format!("{base}/{rest}"))
    })
}

/// A fake smart-HTTP server for the probe's tests.
#[cfg(test)]
pub mod testserver {
    use std::io::{BufRead, Write};

    /// How the server answers every request.
    #[derive(Clone)]
    pub enum Mode {
        /// The ref advertisement, as a public repo's.
        Public,
        /// A 401 asking for credentials, as a private repo's.
        Private,
        /// A 301 to this `Location`.
        Moved(String),
        /// GitLab's: a repo asked without `.git` redirects to its `.git`
        /// form (a relative `Location`), which answers.
        DotGit,
        /// A 200 with a web page.
        Page,
        /// Accepts the connection and never answers.
        Silent,
    }

    /// Serve `mode` at `https://<host>/` (and http) for probes
    /// ([`super::ROUTES`]); the requests' header blocks come back on the
    /// receiver as they arrive.
    pub fn serve(host: &str, mode: Mode) -> std::sync::mpsc::Receiver<String> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        super::ROUTES.lock().unwrap().insert(host.to_owned(), format!("http://127.0.0.1:{port}"));
        let (send, recv) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                let path = request.split_whitespace().nth(1).unwrap_or("").to_owned();
                let _ = send.send(request);
                let mut stream = stream;
                let advertisement = "001e# service=git-upload-pack\n0000000eversion 2\n0000";
                let public = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-git-upload-pack-advertisement\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{advertisement}",
                    advertisement.len()
                );
                let moved = |to: &str| {
                    format!("HTTP/1.1 301 Moved Permanently\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                };
                let response = match &mode {
                    Mode::Public => public,
                    Mode::Private => "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\n\
                                      Content-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned(),
                    Mode::Moved(to) => moved(to),
                    Mode::DotGit if path.contains(".git/info/refs") => public,
                    Mode::DotGit => moved(&path.replacen("/info/refs", ".git/info/refs", 1)),
                    Mode::Page => "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 6\r\n\
                                   Connection: close\r\n\r\n<html>"
                        .to_owned(),
                    Mode::Silent => {
                        held.push(stream);
                        continue;
                    }
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        recv
    }
}

#[cfg(test)]
mod tests {
    use super::testserver::{serve, Mode};
    use super::*;

    fn check_now(url: &str) -> Verdict {
        check_remote(url, AskWindow::now()).unwrap()
    }

    fn refused(verdict: &Verdict) -> &str {
        match verdict {
            Verdict::Refuse(message) => message,
            Verdict::Publish(url) => panic!("published {url}"),
        }
    }

    /// A public https remote is published as written, an ssh one as its
    /// https equivalent; the request carries no credentials of any kind
    /// and says it is git.
    #[test]
    fn a_public_remote_is_published_and_asked_anonymously() {
        let requests = serve("public.probe.test", Mode::Public);
        assert_eq!(check_now("https://public.probe.test/org/r"), Verdict::Publish("https://public.probe.test/org/r".to_owned()));
        let request = requests.recv().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /org/r/info/refs?service=git-upload-pack "), "{request}");
        assert!(request.contains("user-agent: git/cactup-"), "{request}");
        for header in ["authorization", "cookie", "referer", "proxy-authorization"] {
            assert!(!request.contains(&format!("{header}:")), "{request}");
        }
        assert_eq!(check_now("git@public.probe.test:org/r.git"), Verdict::Publish("https://public.probe.test/org/r.git".to_owned()));
    }

    /// An https remote that asks for credentials (401), serves a web page,
    /// moved, or never answers is refused in the https wording, what it did
    /// said; git:// is refused by its form, unasked.
    #[test]
    fn an_https_remote_that_does_not_answer_is_refused() {
        serve("private.probe.test", Mode::Private);
        let private = check_now("https://private.probe.test/org/r.git");
        let said = refused(&private);
        assert!(said.contains("https://private.probe.test/org/r.git doesn't answer git without credentials"), "{said}");
        assert!(said.ends_with("(asked without credentials, it answered 401 Unauthorized)"), "{said}");
        serve("page.probe.test", Mode::Page);
        assert!(refused(&check_now("https://page.probe.test/org/r")).contains("not as a git server"));
        serve("moved.probe.test", Mode::Moved("https://moved.probe.test/new/r.git/info/refs?service=git-upload-pack".to_owned()));
        let moved = check_now("https://moved.probe.test/org/r.git");
        assert!(refused(&moved).contains("redirects to https://moved.probe.test/new/r.git: the repo has moved"), "{moved:?}");
        serve("silent.probe.test", Mode::Silent);
        let short = AskWindow::from(Instant::now(), Duration::from_millis(600));
        let started = Instant::now();
        let silent = check_remote("https://silent.probe.test/org/r.git", short).unwrap();
        assert_eq!(
            refused(&silent),
            "https://silent.probe.test/org/r.git can't be checked: its host couldn't be reached (no answer within 1 s); try \
             again later"
        );
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        let before = PROBED.lock().unwrap().len();
        assert!(refused(&check_now("git://github.com/o/r.git")).contains("git://"));
        assert!(!PROBED.lock().unwrap()[before..].iter().any(|u| u.contains("github.com/o/r")));
    }

    /// GitLab's redirect to the same repo's `.git` form is asked once more,
    /// and when that answers, the candidate is published, not the target.
    #[test]
    fn a_redirect_to_the_same_repo_is_asked_once_more_and_the_candidate_published() {
        let requests = serve("gitlab.probe.test", Mode::DotGit);
        assert_eq!(check_now("https://gitlab.probe.test/g/p"), Verdict::Publish("https://gitlab.probe.test/g/p".to_owned()));
        let paths: Vec<String> = requests.try_iter().map(|r| r.split_whitespace().nth(1).unwrap().to_owned()).collect();
        assert_eq!(paths, ["/g/p/info/refs?service=git-upload-pack", "/g/p.git/info/refs?service=git-upload-pack"]);
    }

    /// A host that does not resolve here is not refused for that: it is
    /// probed (through a proxy it may answer), and published when it
    /// answers; refused, when nothing answers, for the failed connection,
    /// not for the resolution.
    #[test]
    fn a_host_that_does_not_resolve_here_is_probed_not_refused() {
        UNRESOLVED.lock().unwrap().insert("unresolved.probe.test".to_owned());
        let asked = serve("unresolved.probe.test", Mode::Public);
        assert_eq!(check_now("https://unresolved.probe.test/o/r"), Verdict::Publish("https://unresolved.probe.test/o/r".to_owned()));
        assert!(asked.try_recv().is_ok(), "asked");
        let nowhere = check_now("https://nowhere.invalid/o/r.git");
        assert_eq!(
            refused(&nowhere),
            "https://nowhere.invalid/o/r.git can't be checked: its host couldn't be reached (dns lookup failed); try again later"
        );
    }

    /// A remote at, or resolving to, a private address is refused unasked,
    /// saying so.
    #[test]
    fn a_private_address_is_refused() {
        assert_eq!(
            refused(&check_now("https://127.0.0.1/org/r.git")),
            "https://127.0.0.1/org/r.git can't be checked: 127.0.0.1 is not a public address, so nobody outside your \
             network can fetch it; publish a URL on a public host"
        );
        assert_eq!(
            refused(&check_now("git@[::1]:o/r.git")),
            "git@[::1]:o/r.git can't be checked: [::1] is not a public address, so nobody outside your network can fetch \
             it; publish a URL on a public host"
        );
        assert!(refused(&check_now("http://localhost:9/org/r.git")).contains("localhost is not a public address"));
        // A connection refused is said by its kind, with no URL in the detail.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        ROUTES.lock().unwrap().insert("closed.probe.test".to_owned(), format!("http://127.0.0.1:{closed}"));
        assert_eq!(
            refused(&check_now("https://closed.probe.test/o/r")),
            "https://closed.probe.test/o/r can't be checked: its host couldn't be reached (connection refused); try again later"
        );
    }
}
