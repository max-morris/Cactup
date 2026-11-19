//! Whether a repo answers git over https with no credentials at all: what
//! decides whether an ssh remote may be published as its https equivalent
//! (`cactup_ecosystem_common::url::shareability`, spec §8.9).
//!
//! The probe asks what `git ls-remote` asks first, the smart-HTTP ref
//! advertisement (`<url>/info/refs?service=git-upload-pack`), with reqwest
//! directly rather than gix: gix consults the user's credential helpers
//! (even with prompts off), `.netrc` and `http.extraHeader`, and a private
//! repo the user happens to have access to would look public and be
//! published to people who cannot fetch it. This client sends nothing that
//! could identify anyone: no `Authorization` header, no cookies (it keeps
//! none), no `.netrc` (reqwest reads none), no `Referer`, and it follows no
//! redirect, so nothing reaches any host but the one asked. A proxy from
//! the environment is used as for any download; its credentials, if any,
//! go to the proxy alone.
//!
//! Only the advertisement counts as an answer: a 200 with git's content
//! type. A redirect is the repo having moved (GitHub answers a renamed one
//! with a 301); anything else (a 401 or 403 asking for credentials, a 404,
//! a web page, a silence, a failed connection) is no anonymous answer.
//!
//! **This probe must stay identical to the Cactarium's** (its PR #14): the
//! same request, and the same answers counted as answers, or a borderline
//! host would be accepted by one side and refused by the other. Change
//! both or neither.

use super::git::AskWindow;
use super::watch::{abandonable, Bounds, Stop};
use crate::Res;
use anyhow::bail;
use std::time::{Duration, Instant};

/// What a repo URL is followed by to ask for its ref advertisement; also
/// what is cut off a redirect's `Location` to name where the repo went.
pub const ADVERTISEMENT: &str = "/info/refs?service=git-upload-pack";

/// The content type of a smart-HTTP ref advertisement.
const CONTENT_TYPE: &str = "application/x-git-upload-pack-advertisement";

/// The longest a probe waits to connect (within what is left of its window).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How a repo's https URL answered a probe with no credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// With its ref advertisement: anyone can fetch it.
    Answered,
    /// With a redirect: to where its `Location` pointed, without the
    /// smart-HTTP suffix (`None`: no `Location`).
    Moved(Option<String>),
    /// Not with the advertisement, and how, as a clause after the URL:
    /// "answered 401 Unauthorized", "did not answer within 30 s", ….
    Refused(String),
}

/// Ask `https` for its ref advertisement with no credentials, within
/// `window` and Ctrl-C: the request runs on a thread of its own
/// ([`super::watch`]), and a Ctrl-C fails the probe at once
/// ("Interrupted"), the only error; a silence that outlasts the window is
/// [`Answer::Refused`].
pub fn probe(https: &str, window: AskWindow) -> Res<Answer> {
    let left = window.until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Ok(Answer::Refused("was not asked: the time for asking was up".to_owned()));
    }
    #[cfg(test)]
    PROBED.lock().expect("test record poisoned").push(https.to_owned());
    let owned = https.to_owned();
    // The request reports no progress: the silence is the whole probe. Its
    // own timeout is a little longer than the watch's, so a silence is
    // said as the window, and the thread ends soon after on its own.
    let bounds = Bounds { stall: left, total: Some(left), wind_down: Duration::ZERO };
    match abandonable(gix::progress::Discard, bounds, move |_, _| ask(&owned, left + Duration::from_secs(1))) {
        Ok(answer) => Ok(answer),
        Err(Stop::Failed(e)) => Ok(Answer::Refused(format!("could not be asked: {e:#}"))),
        Err(Stop::Interrupted) => bail!("Interrupted"),
        Err(Stop::Stalled(_) | Stop::Overran(_)) => {
            let secs = (window.length.as_secs_f64().round() as u64).max(1);
            Ok(Answer::Refused(format!("did not answer within {secs} s")))
        }
        Err(Stop::Vanished) => Ok(Answer::Refused("could not be asked".to_owned())),
    }
}

/// The request itself, on the probe's thread: a bare GET of the
/// advertisement, as the Cactarium's (see the module docs: keep the two
/// identical).
fn ask(https: &str, timeout: Duration) -> Res<Answer> {
    use reqwest::header;
    let target = format!("{}{ADVERTISEMENT}", https.trim_end_matches('/'));
    #[cfg(test)]
    let target = routed(&target);
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .user_agent(concat!("git/cactup-", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .timeout(timeout)
        .build()?;
    let response = match client.get(&target).send() {
        Ok(response) => response,
        Err(e) => return Ok(Answer::Refused(format!("could not be reached: {:#}", anyhow::Error::from(e)))),
    };
    let status = response.status();
    if status.is_redirection() {
        let to = response
            .headers()
            .get(header::LOCATION)
            .and_then(|l| l.to_str().ok())
            .and_then(|l| response.url().join(l).ok())
            .map(|u| u.as_str().strip_suffix(ADVERTISEMENT).unwrap_or(u.as_str()).to_owned());
        return Ok(Answer::Moved(to));
    }
    if status != reqwest::StatusCode::OK {
        return Ok(Answer::Refused(format!("answered {status}")));
    }
    let kind = response.headers().get(header::CONTENT_TYPE).and_then(|t| t.to_str().ok()).unwrap_or("").to_owned();
    if !kind.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case(CONTENT_TYPE) {
        let kind = if kind.is_empty() { "no content type".to_owned() } else { kind };
        return Ok(Answer::Refused(format!("answered, but not as a git server ({kind})")));
    }
    Ok(Answer::Answered)
}

/// Every https URL probed in this process, in order (tests only).
#[cfg(test)]
pub static PROBED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Where a test's https host is served instead: `https://<host>` → a local
/// `http://127.0.0.1:<port>` ([`testserver::serve`]), since a probe always
/// asks https and an ssh URL's port does not survive into it. Tests use
/// hosts of their own, so they can run side by side.
#[cfg(test)]
pub static ROUTES: std::sync::Mutex<std::collections::BTreeMap<String, String>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

#[cfg(test)]
fn routed(target: &str) -> String {
    let routes = ROUTES.lock().expect("test routes poisoned");
    for (host, base) in routes.iter() {
        if let Some(rest) = target.strip_prefix(&format!("https://{host}/")) {
            return format!("{base}/{rest}");
        }
    }
    target.to_owned()
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
        /// A 200 with a web page.
        Page,
        /// Accepts the connection and never answers.
        Silent,
    }

    /// Serve `mode` at `https://<host>/` for probes ([`super::ROUTES`]);
    /// the requests' header blocks come back on the receiver as they arrive.
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
                let _ = send.send(request);
                let mut stream = stream;
                let advertisement = "001e# service=git-upload-pack\n0000000eversion 2\n0000";
                let response = match &mode {
                    Mode::Public => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/x-git-upload-pack-advertisement\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{advertisement}",
                        advertisement.len()
                    ),
                    Mode::Private => "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\n\
                                      Content-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned(),
                    Mode::Moved(to) => format!(
                        "HTTP/1.1 301 Moved Permanently\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
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

    fn window() -> AskWindow {
        AskWindow::now()
    }

    /// A public repo answers; the request carries no credentials of any
    /// kind and says it is git.
    #[test]
    fn a_public_repo_answers_and_is_asked_anonymously() {
        let requests = serve("public.probe.test", Mode::Public);
        assert_eq!(probe("https://public.probe.test/org/r.git", window()).unwrap(), Answer::Answered);
        let request = requests.recv().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /org/r.git/info/refs?service=git-upload-pack "), "{request}");
        assert!(request.contains("user-agent: git/"), "{request}");
        for header in ["authorization", "cookie", "referer", "proxy-authorization"] {
            assert!(!request.contains(&format!("{header}:")), "{request}");
        }
    }

    /// A 401, a web page and a silence are no answer, each said; a 301 is a
    /// move, to its `Location` without the smart-HTTP suffix.
    #[test]
    fn anything_but_the_advertisement_is_no_answer() {
        serve("private.probe.test", Mode::Private);
        let refused = probe("https://private.probe.test/org/r.git", window()).unwrap();
        assert_eq!(refused, Answer::Refused("answered 401 Unauthorized".to_owned()));
        serve("page.probe.test", Mode::Page);
        let page = probe("https://page.probe.test/org/r", window()).unwrap();
        assert!(matches!(&page, Answer::Refused(why) if why.contains("not as a git server")), "{page:?}");
        serve("moved.probe.test", Mode::Moved(format!("https://moved.probe.test/new/r.git{ADVERTISEMENT}")));
        let moved = probe("https://moved.probe.test/org/r.git", window()).unwrap();
        assert_eq!(moved, Answer::Moved(Some("https://moved.probe.test/new/r.git".to_owned())));
        serve("elsewhere.probe.test", Mode::Moved("https://elsewhere.probe.test/sign_in".to_owned()));
        let elsewhere = probe("https://elsewhere.probe.test/org/r.git", window()).unwrap();
        assert_eq!(elsewhere, Answer::Moved(Some("https://elsewhere.probe.test/sign_in".to_owned())));
        serve("silent.probe.test", Mode::Silent);
        let short = AskWindow::from(Instant::now(), Duration::from_millis(600));
        let started = Instant::now();
        let silent = probe("https://silent.probe.test/org/r.git", short).unwrap();
        assert_eq!(silent, Answer::Refused("did not answer within 1 s".to_owned()));
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    }
}
