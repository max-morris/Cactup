//! http/https/ftp components: plain downloads of `$URL/$CHECKOUT` into
//! `$TARGET`, mirroring GetComponents' `handle_wget` (no archive extraction).
//! Implemented by the FETCH stream.
//!
//! Ported from `Cactus/bin/GetComponents`'s `handle_wget` (lines ~2165-2263;
//! `!TYPE = http`/`https`/`ftp` all dispatch to it, line ~120-122). The Perl
//! shells out to `wget`, letting *it* decide the saved filename from the
//! fetched URL when `!NAME` is absent; we fetch with `reqwest` instead (no
//! new process, no dependence on a `wget` binary being installed) and so
//! must decide that filename ourselves — [`derive_filename`] mirrors wget's
//! usual behavior (the URL's last path segment) closely enough for every
//! real thornlist entry, without trying to replicate wget's exact
//! `Content-Disposition`/redirect-chain heuristics.
//!
//! Every request carries a `User-Agent` ([`USER_AGENT`]) — `wget` sent its
//! own, reqwest sends none, and some mirrors 403 a request without one.
//!
//! Deliberate divergence from GetComponents: `!TYPE = ftp` is rejected
//! outright. `reqwest` (rustls-backed, see `Cargo.toml`) speaks http/https
//! only; GetComponents' own `ftp` support was really just `wget`'s (line
//! ~121: `'ftp' => \&handle_wget`), which cactup has no equivalent for
//! without shelling out. Callers should fetch an `ftp` component manually
//! (e.g. with `wget`/`curl`/a browser) into the destination named in the
//! error, or install it another way.

use crate::thornlist::{Component, ComponentType};
use anyhow::{anyhow, bail, Context};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// The `User-Agent` every cactup download presents. reqwest sends none by
/// default, and a bare request is not merely impolite: some mirrors reject
/// it outright — `ftp.gnu.org`, which real thornlists do point at, answers
/// one with a 403 that reads like the file is gone.
///
/// Identifies cactup and its version, the two things a mirror operator
/// looking at a log needs: `cactup/<version>+<build id>`, or `+dev` for a
/// dev build (see [`crate::build_info`]).
const USER_AGENT: &str = crate::build_info::USER_AGENT;

/// One client for every download in a run, built once. Downloads run four
/// at a time and a thornlist's downloads often share a host, so this is also
/// what lets them reuse a connection instead of repeating a TLS handshake
/// per file — and it builds rustls' root store once rather than per URL.
///
/// A failure here means the process cannot speak HTTPS at all (no usable TLS
/// backend), which no individual download could recover from, so it panics
/// rather than making every call site carry the error.
static CLIENT: LazyLock<reqwest::blocking::Client> = LazyLock::new(|| {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .expect("failed to build the HTTP client")
});

/// Download one http/https component into `<install_root>/<target>/`,
/// overwriting whatever is already there (refetch semantics: downloads are
/// always refreshed, matching GetComponents' `update` method never skipping
/// a re-fetch). Returns the final file path. Never extracts an archive —
/// neither does `handle_wget`. Streams the body in 64 KiB chunks rather than
/// buffering it whole, reporting bytes (with a percentage and throughput,
/// when the server sends `Content-Length`) via `progress`.
pub fn download_component(
    install_root: &Path,
    component: &Component,
    progress: &mut impl prodash::Progress,
) -> crate::Res<PathBuf> {
    if component.ty == ComponentType::Ftp {
        let dest_dir = install_root.join(&component.target);
        bail!(
            "component '{}' uses !TYPE = ftp, which cactup cannot fetch natively \
             (reqwest, cactup's HTTP client, speaks only http/https — GetComponents \
             itself only handled ftp by shelling out to wget, see the module docs). \
             Fetch it manually into '{}' or install the component another way.",
            component.checkout,
            dest_dir.display(),
        );
    }
    if !matches!(component.ty, ComponentType::Http | ComponentType::Https) {
        bail!(
            "download_component called on component '{}' with !TYPE {:?}, which is \
             not a download type (caller bug)",
            component.checkout,
            component.ty,
        );
    }

    let url = component
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("component '{}' has no !URL", component.checkout))?;
    let fetch_url = effective_url(url, &component.checkout);
    let filename = derive_filename(component.name.as_deref(), &component.checkout);

    let dest_dir = install_root.join(&component.target);
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("Failed to create {}", dest_dir.display()))?;
    let dest_path = dest_dir.join(&filename);
    let tmp_path = dest_dir.join(format!("{filename}.part"));

    // Write to a same-directory temp file, then rename into place, so a
    // reader never observes a partially-written destination file.
    {
        let mut tmp_file = std::fs::File::create(&tmp_path)
            .with_context(|| format!("Failed to create {}", tmp_path.display()))?;
        download_to(client(), &fetch_url, &mut tmp_file, progress)?;
    }
    std::fs::rename(&tmp_path, &dest_path).with_context(|| {
        format!("Failed to move {} into place at {}", tmp_path.display(), dest_path.display())
    })?;

    Ok(dest_path)
}

/// The shared download client ([`CLIENT`]): cactup's `User-Agent` and
/// reqwest's default 30 s timeout on each connect, read and write.
pub(crate) fn client() -> &'static reqwest::blocking::Client {
    &CLIENT
}

/// GET `url` with `client` and stream the body into `dest`, returning the
/// number of bytes written. Streamed in 64 KiB chunks rather than buffered
/// whole, so progress (and memory use) tracks the download as it happens
/// rather than jumping to 100% at the end: `progress` is initialized in
/// bytes (with a percentage and throughput when the server sends
/// `Content-Length`) and advanced per chunk. The interrupt flag is polled
/// per chunk, failing with "interrupted". A non-success status is an error
/// that carries the `reqwest::Error`, and so the status, in its chain.
pub(crate) fn download_to(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &mut std::fs::File,
    progress: &mut impl prodash::Progress,
) -> crate::Res<u64> {
    let mut response = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("Failed to download {url}"))?;
    progress.init(
        response.content_length().map(|l| l as usize),
        Some(prodash::unit::dynamic_and_mode(
            prodash::unit::Bytes,
            prodash::unit::display::Mode::with_throughput().and_percentage(),
        )),
    );

    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        if gix::interrupt::is_triggered() {
            bail!("interrupted");
        }
        let n = std::io::Read::read(&mut response, &mut buf)
            .with_context(|| format!("Failed to read the response body of {url}"))?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(dest, &buf[..n])
            .with_context(|| format!("Failed to save the download of {url}"))?;
        total += n as u64;
        progress.inc_by(n);
    }
    Ok(total)
}

/// `"$url/$checkout"` (GetComponents lines 2193, 2198, 2221, 2226): a plain
/// string join, not a URL-aware one. If `checkout` is `.` or empty, the
/// result ends in a literal `/.` or `/` respectively — GetComponents never
/// special-cases either; whatever the join produces is handed straight to
/// the fetcher (wget there, reqwest here).
fn effective_url(url: &str, checkout: &str) -> String {
    format!("{url}/{checkout}")
}

/// The saved filename: `!NAME` if present (GetComponents renames the
/// wget-fetched file to it, lines 2183-2195), else the last `/`-separated
/// segment of `checkout` — matching wget's own default of naming the file
/// after the fetched URL's last path segment, since `checkout` is exactly
/// what gets appended last in [`effective_url`]. Falls back to `index.html`
/// (wget's own fallback for a URL with no usable trailing segment) when that
/// segment is empty or `.` — i.e. `checkout` is empty or `.`.
fn derive_filename(name: Option<&str>, checkout: &str) -> String {
    if let Some(n) = name {
        return n.to_owned();
    }
    let base = checkout.rsplit('/').next().unwrap_or(checkout);
    if base.is_empty() || base == "." {
        "index.html".to_owned()
    } else {
        base.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A standalone progress item — no renderer, just the in-memory counters
    /// `download_component` writes to.
    fn test_progress() -> prodash::tree::Item {
        prodash::tree::Root::new().add_child("test")
    }

    /// A one-shot HTTP server on loopback. Answers the first request with
    /// `body` and returns the request head it received, so a test can assert
    /// what actually went out on the wire rather than what we meant to send.
    /// Hand-rolled on `std::net`: proving one header is sent does not justify
    /// a dev-dependency on an HTTP server.
    fn serve_once(body: &'static [u8]) -> (String, std::thread::JoinHandle<String>) {
        let (base, server) = test_server::serve(vec![("*", body.to_vec())], 1);
        let handle = std::thread::spawn(move || server.join().expect("server thread").remove(0));
        (base, handle)
    }

    #[test]
    fn a_download_identifies_itself_by_name_and_version() {
        // The header itself: reqwest sends none by default, and a mirror that
        // rejects that (ftp.gnu.org 403s it) fails in a way that reads like
        // the file is missing.
        assert!(USER_AGENT.starts_with(&format!("cactup/{}+", crate::VERSION)), "{USER_AGENT}");

        let (base, server) = serve_once(b"payload");
        let dir = tempfile::tempdir().unwrap();
        let mut c = http_component("file.txt", None);
        c.url = Some(base);
        let path = download_component(dir.path(), &c, &mut test_progress()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");

        let head = server.join().expect("server thread");
        let sent = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
            .map(|(_, value)| value.trim().to_owned());
        assert_eq!(sent.as_deref(), Some(USER_AGENT), "request head was:\n{head}");
    }

    fn http_component(checkout: &str, name: Option<&str>) -> Component {
        Component {
            ty: ComponentType::Http,
            target: "Cactus/arrangements/Thorns".to_owned(),
            checkout: checkout.to_owned(),
            name: name.map(str::to_owned),
            url: Some("https://example.com/dist".to_owned()),
            auth_url: None,
            anon_user: None,
            anon_pass: None,
            repo_path: None,
            branch: None,
            repo: "dist".to_owned(),
        }
    }

    #[test]
    fn effective_url_is_a_plain_join() {
        assert_eq!(effective_url("https://example.com", "file.tar.gz"), "https://example.com/file.tar.gz");
        assert_eq!(effective_url("https://example.com", "."), "https://example.com/.");
        assert_eq!(effective_url("https://example.com", ""), "https://example.com/");
    }

    #[test]
    fn derive_filename_prefers_name() {
        assert_eq!(derive_filename(Some("custom.tar.gz"), "file.tar.gz"), "custom.tar.gz");
    }

    #[test]
    fn derive_filename_uses_checkout_basename() {
        assert_eq!(derive_filename(None, "file.tar.gz"), "file.tar.gz");
        assert_eq!(derive_filename(None, "sub/dir/file.tar.gz"), "file.tar.gz");
    }

    #[test]
    fn derive_filename_falls_back_for_dot_or_empty_checkout() {
        assert_eq!(derive_filename(None, "."), "index.html");
        assert_eq!(derive_filename(None, ""), "index.html");
    }

    #[test]
    fn ftp_type_bails_without_touching_network() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = http_component("file.tar.gz", None);
        c.ty = ComponentType::Ftp;
        let err = download_component(dir.path(), &c, &mut test_progress()).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("ftp"), "{message}");
        assert!(message.contains("file.tar.gz"), "{message}");
    }

    #[test]
    fn wrong_type_is_a_caller_bug_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = http_component("file.tar.gz", None);
        c.ty = ComponentType::Git;
        let err = download_component(dir.path(), &c, &mut test_progress()).unwrap_err();
        assert!(format!("{err:#}").contains("caller bug"));
    }

    #[test]
    fn missing_url_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = http_component("file.tar.gz", None);
        c.url = None;
        let err = download_component(dir.path(), &c, &mut test_progress()).unwrap_err();
        assert!(format!("{err:#}").contains("!URL"));
    }
}

/// A loopback HTTP server for tests of code that downloads. Hand-rolled on
/// `std::net`: proving what goes out on the wire does not justify a
/// dev-dependency on an HTTP server.
#[cfg(test)]
pub(crate) mod test_server {
    use std::io::{Read, Write};

    /// Serve exactly `requests` requests, one per connection, then stop.
    /// Each `(path, body)` route answers a GET of that path (`"*"` answers
    /// any path) with a 200 and the body; any other path gets a 404. The
    /// join handle yields every request head received, in order, so a test
    /// can assert what actually went out rather than what it meant to send.
    /// Returns the base URL (`http://127.0.0.1:<port>`) and that handle.
    pub(crate) fn serve(
        routes: Vec<(&'static str, Vec<u8>)>,
        requests: usize,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        serve_with(requests, move |path| match routes.iter().find(|(p, _)| *p == "*" || *p == path) {
            Some((_, body)) => ("200 OK", String::new(), body.clone()),
            None => ("404 Not Found", String::new(), b"not found".to_vec()),
        })
    }

    /// Like [`serve`], but every request is answered with a 302 to `location`.
    pub(crate) fn redirect(
        location: String,
        requests: usize,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        serve_with(requests, move |_| ("302 Found", format!("Location: {location}\r\n"), Vec::new()))
    }

    /// Serve `requests` requests, answering each with what `reply` returns
    /// for its path: a status, extra header lines (each ending in CRLF) and
    /// a body.
    fn serve_with(
        requests: usize,
        reply: impl Fn(&str) -> (&'static str, String, Vec<u8>) + Send + 'static,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let handle = std::thread::spawn(move || {
            let mut heads = Vec::new();
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut head = Vec::new();
                let mut buf = [0u8; 512];
                // Read to the end of the request head; a GET has no body to
                // wait for.
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("");
                let (status, headers, body) = reply(path);
                let reply = format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).expect("write status");
                stream.write_all(&body).expect("write body");
                heads.push(head);
            }
            heads
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }
}
