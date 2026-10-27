//! Retry with backoff for the network half of a fetch: one dropped
//! connection on the way to one of eighty git hosts must not fail a whole
//! install.
//!
//! Only a failure [`transient`] recognizes is retried — the connection
//! failed, timed out or broke off, or the server said it cannot answer right
//! now. Anything else (a 404, a 401, a branch the remote does not have, a
//! malformed URL) fails at once, as it always did: asking again cannot change
//! the answer, and waiting half a minute to report a typo is its own harm.
//!
//! [`with_backoff`] runs its closure again from the top, so callers hand it
//! only work that leaves nothing behind when it fails: a whole clone (gix
//! deletes the directory a failed clone created), the fetch half of an
//! align (nothing in the repo moves until the pack is in), a download into a
//! temp file the next attempt truncates. Never a checkout.

use crate::Res;
use std::time::{Duration, Instant};

/// The waits before each retry: four retries, about 30 s in all before a
/// component is given up on. Each is stretched or shrunk by up to a quarter
/// so the fetch workers, all knocked over by the same outage, do not come
/// back at the host in lockstep. (Milliseconds under test, so a test can
/// retry for real without sleeping through it.)
const BACKOFF: [Duration; 4] = if cfg!(test) {
    [Duration::from_millis(2), Duration::from_millis(4), Duration::from_millis(8), Duration::from_millis(16)]
} else {
    [Duration::from_secs(2), Duration::from_secs(4), Duration::from_secs(8), Duration::from_secs(16)]
};

/// How often a wait checks for Ctrl-C (§2.4: the wind-down must feel
/// instant).
const POLL: Duration = Duration::from_millis(50);

/// Run `attempt`, and run it again after a pause each time it fails with a
/// [`transient`] error, up to [`BACKOFF`]'s length. The line counts each
/// pause down (`retry 1 of 4 in 3s`) in place of the phase, puts the phase
/// back when the next attempt starts, and resets the bar a failed attempt
/// left part-filled. Ctrl-C ends a pause at once, failing with the error that
/// started it.
pub(crate) fn with_backoff<P: prodash::Progress, T>(
    progress: &mut P,
    mut attempt: impl FnMut(&mut P) -> Res<T>,
) -> Res<T> {
    let phase = progress.name();
    let mut delays = BACKOFF.iter().enumerate();
    loop {
        let error = match attempt(progress) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if gix::interrupt::is_triggered() || !transient(&error) {
            return Err(error);
        }
        let Some((retry, &delay)) = delays.next() else {
            return Err(error.context(format!("gave up after {} attempts", BACKOFF.len() + 1)));
        };
        progress.set(0);
        if !count_down(progress, jittered(delay), retry + 1) {
            return Err(error.context("interrupted while waiting to retry"));
        }
        if let Some(phase) = &phase {
            progress.set_name(phase.clone());
        }
    }
}

/// `delay`, scaled by a random factor in [0.75, 1.25).
fn jittered(delay: Duration) -> Duration {
    delay.mul_f64(0.75 + fastrand::f64() / 2.0)
}

/// Sleep for `delay`, showing the seconds left on the line; `false` when
/// Ctrl-C cut it short.
fn count_down(progress: &mut impl prodash::Progress, delay: Duration, retry: usize) -> bool {
    let until = Instant::now() + delay;
    let mut shown = None;
    loop {
        if gix::interrupt::is_triggered() {
            return false;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        let secs = left.as_secs_f64().ceil() as u64;
        if shown != Some(secs) {
            progress.set_name(format!("retry {retry} of {} in {secs}s", BACKOFF.len()));
            shown = Some(secs);
        }
        std::thread::sleep(left.min(POLL));
    }
}

/// Whether asking again might go differently. Decided by the first I/O or
/// HTTP error in `error`'s chain — the bottom of a gix transport failure, or
/// the reqwest error of a download — and `false` when there is none: an
/// error that never touched the network is not one the network can fix.
pub(crate) fn transient(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| {
            if let Some(e) = cause.downcast_ref::<reqwest::Error>() {
                return Some(transient_http(e));
            }
            cause.downcast_ref::<std::io::Error>().map(transient_io)
        })
        .unwrap_or(false)
}

fn transient_http(e: &reqwest::Error) -> bool {
    match e.status() {
        Some(status) => transient_status(status.as_u16()),
        // No answer at all: the connection (DNS, TCP, TLS) failed or timed
        // out, or the body broke off part way.
        None => e.is_connect() || e.is_timeout() || e.is_request() || e.is_body(),
    }
}

/// Request Timeout, Too Many Requests, and the server-side failures.
fn transient_status(code: u16) -> bool {
    code == 408 || code == 429 || (500..600).contains(&code)
}

fn transient_io(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    // reqwest's blocking body reader reports a failed read this way.
    if let Some(e) = e.get_ref().and_then(|inner| inner.downcast_ref::<reqwest::Error>()) {
        return transient_http(e);
    }
    match e.kind() {
        TimedOut | ConnectionReset | ConnectionAborted | ConnectionRefused | NotConnected | BrokenPipe
        | UnexpectedEof | Interrupted | HostUnreachable | NetworkUnreachable | NetworkDown => true,
        // gix's reqwest transport flattens a failed request into a message
        // under `Other` (gix-transport, `http/reqwest/remote.rs`): "Received
        // HTTP status 404" for a status it gives no kind of its own (401 is
        // `PermissionDenied`, 5xx `ConnectionAborted`), and the reqwest
        // error's own text — "error sending request for url (…)", whatever
        // the cause underneath — for a request that never got an answer.
        // It also wraps a typed gix error under `Other` once (a reply with
        // the wrong content type, a server that does not speak git): that
        // one is permanent.
        Other => match e.to_string().strip_prefix("Received HTTP status ") {
            Some(code) => code.trim().parse().is_ok_and(transient_status),
            None => !e
                .get_ref()
                .is_some_and(|inner| inner.is::<gix::protocol::transport::client::Error>()),
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    fn io(kind: std::io::ErrorKind, message: &str) -> anyhow::Error {
        Err::<(), _>(std::io::Error::new(kind, message.to_owned()))
            .context("Failed to fetch https://example.invalid/repo.git (branch <default>)")
            .unwrap_err()
    }

    #[test]
    fn transient_reads_the_io_error_under_the_context() {
        use std::io::ErrorKind::*;
        // What gix's reqwest transport makes of a request that never got an
        // answer — the failure in the bug report this module answers.
        assert!(transient(&io(Other, "error sending request for url (https://example.invalid/info/refs)")));
        assert!(transient(&io(ConnectionAborted, "Received HTTP status 503")));
        assert!(transient(&io(Other, "Received HTTP status 429")));
        assert!(transient(&io(ConnectionReset, "connection reset by peer")));
        assert!(transient(&io(TimedOut, "timed out")));

        assert!(!transient(&io(Other, "Received HTTP status 404")));
        assert!(!transient(&io(PermissionDenied, "Received HTTP status 401")));
        assert!(!transient(&io(NotFound, "no such file")));
        assert!(!transient(&anyhow::anyhow!("origin/main missing after fetch")));
    }

    /// A loopback server answering gix's one request (the ref
    /// advertisement) with `status`.
    fn serve(status: &'static str) -> String {
        let (base, _server) =
            crate::fetch::download::test_server::serve_with(1, move |_| (status, String::new(), Vec::new()));
        format!("{base}/repo.git")
    }

    /// The error a real gix clone of `url` fails with.
    fn clone_error(url: &str) -> anyhow::Error {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("repo");
        let mut progress = prodash::progress::Discard;
        let prepare = gix::prepare_clone(url, &dest).unwrap();
        let error = prepare
            .with_shallow(gix::remote::fetch::Shallow::DepthAtRemote(1.try_into().unwrap()))
            .fetch_then_checkout(&mut progress, &gix::interrupt::IS_INTERRUPTED)
            .map(drop)
            .with_context(|| format!("Failed to fetch {url}"))
            .unwrap_err();
        assert!(!dest.exists(), "a failed clone leaves nothing to trip over the retry");
        error
    }

    /// The classification against what gix actually produces, not against
    /// errors shaped by hand: the transport's own wrapping is what decides
    /// whether `transient` can see the cause at all.
    #[test]
    fn transient_classifies_real_gix_transport_failures() {
        // Nothing listening: the connection is refused.
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let refused = clone_error(&format!("http://127.0.0.1:{closed}/repo.git"));
        assert!(transient(&refused), "{refused:#}");

        let unavailable = clone_error(&serve("503 Service Unavailable"));
        assert!(transient(&unavailable), "{unavailable:#}");

        let missing = clone_error(&serve("404 Not Found"));
        assert!(!transient(&missing), "{missing:#}");

        // A 200 that is not git's smart protocol: a login page, a web UI.
        let not_git = clone_error(&serve("200 OK"));
        assert!(!transient(&not_git), "{not_git:#}");
    }

    #[test]
    fn with_backoff_retries_only_what_is_transient() {
        let mut progress = prodash::progress::Discard;

        let mut calls = 0;
        let got = with_backoff(&mut progress, |_| {
            calls += 1;
            Err::<(), _>(io(std::io::ErrorKind::Other, "Received HTTP status 404"))
        });
        assert!(got.is_err());
        assert_eq!(calls, 1, "a permanent failure is not asked again");

        let mut calls = 0;
        let got = with_backoff(&mut progress, |_| {
            calls += 1;
            if calls == 1 {
                Err(io(std::io::ErrorKind::ConnectionReset, "connection reset by peer"))
            } else {
                Ok(calls)
            }
        });
        assert_eq!(got.unwrap(), 2, "a transient failure is retried until it succeeds");
    }
}
