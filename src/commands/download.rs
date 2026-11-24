//! `cactup download submit <url>` (spec §8.10): a download (a tarball a
//! thornlist fetches, a `.sim` input's file) submitted for Cactarium's
//! review, so a `.sim` that pins it can be shared. The file is downloaded
//! once and hashed as it streams (nothing is kept), then its URL and sha256
//! are posted with the user's token. Cactarium verifies the file in the
//! background, then an admin reviews it.
// Plan §6 S9, and its update of 2026-11-16 (the 409 for a known download).

use anyhow::{anyhow, bail};
use colored::Colorize;

use super::Ctx;
use crate::cactarium::{Client, Submitted};
use crate::Res;
use cactup_ecosystem_common::api::DownloadSubmission;
use cactup_ecosystem_common::url::{download_shareability, Redacted};

/// `cactup download submit <url> [--note TEXT]`.
pub fn submit(ctx: &Ctx, url: &str, note: Option<String>) -> Res<()> {
    let client = Client::new(&ctx.db.read()?);
    // Before anything is downloaded: without a token, the download is wasted.
    let token = crate::cactarium::token_for(client.base())?;
    submit_with(&client, &token, url, note)?;
    Ok(())
}

/// [`submit`] to `client` with `token`: the lines it printed.
pub(crate) fn submit_with(client: &Client, token: &str, url: &str, note: Option<String>) -> Res<Vec<String>> {
    if let Some(why) = download_shareability(url).download_refusal(url) {
        bail!("Not submitted: {why}.");
    }
    if note.as_deref().is_some_and(|n| n.contains(['\n', '\r'])) {
        bail!("A note is one line.");
    }
    let (sha256, size) = hash_download(url)?;
    let shown = Redacted(url).to_string();
    let mut said = vec![format!("Downloaded {shown}: {}, sha256 {sha256}.", crate::progress::bytes(size as usize))];
    let submission = DownloadSubmission { url: url.to_owned(), sha256: sha256.clone(), note };
    match client.submit_download(token, &submission)? {
        Submitted::New(review) => {
            said.push(format!("Submitted it to {} for review.", client.base()));
            said.push(review_line(&review.review));
        }
        Submitted::Known(known) => {
            said.push(format!("Already submitted, currently {}.", known.known.review));
            let message = known.error.message.trim();
            if !message.is_empty() && message != "already submitted" {
                said.push(message.to_owned());
            }
        }
    }
    for line in &said {
        println!("{line}");
    }
    Ok(said)
}

/// What a review state means for the download.
fn review_line(review: &str) -> String {
    match review {
        "pending" => "Awaiting verification and review by Cactarium.".to_owned(),
        "approved" => "Approved by Cactarium.".green().to_string(),
        "rejected" => "Rejected by Cactarium.".yellow().to_string(),
        other => format!("Review: {other}."),
    }
}

/// The sha256 of what `url` serves, and its size: streamed through the
/// hash, with a line of progress in bytes. The download runs on a thread of
/// its own so that Ctrl-C ends the wait at once, even while the server is
/// silent; the thread stops at its next chunk.
fn hash_download(url: &str) -> Res<(String, u64)> {
    let name = url.split(['?', '#']).next().unwrap_or(url).rsplit('/').find(|s| !s.is_empty()).unwrap_or("download").to_owned();
    let (progress, renderer) =
        crate::manifest::setup_prodash_with(Some(crate::manifest::progress_level_filter(1)), true);
    let layout = crate::progress::Layout::for_names([name.as_str()]);
    let mut line = crate::progress::Line::counting(progress.add_child(name.clone()), &name, layout);
    line.phase("downloading");
    let (send, receive) = std::sync::mpsc::channel();
    let owned = url.to_owned();
    std::thread::spawn(move || {
        let mut hashing = Hashing(aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256));
        let got = crate::fetch::download::download_to(crate::fetch::download::client(), &owned, &mut hashing, &mut line)
            .map_err(|e| anyhow!(crate::fetch::git::scrub(&format!("{e:#}"), &owned)));
        let hex: String = hashing.0.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect();
        let _ = send.send((got.map(|size| (hex, size)), line));
    });
    let answer = loop {
        if gix::interrupt::is_triggered() {
            break Err(anyhow!("Interrupted"));
        }
        match receive.recv_timeout(std::time::Duration::from_millis(50)) {
            // The line goes quietly: the summary printed next names the file.
            Ok((got, _line)) => break got,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Err(anyhow!("The download died")),
        }
    };
    renderer.shutdown_and_wait();
    answer
}

/// A writer that only hashes what it is given.
struct Hashing(aws_lc_rs::digest::Context);

impl std::io::Write for Hashing {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::registered::fake::{paths, Registry};

    const DATA: &[u8] = b"initial data\n";

    fn sha(bytes: &[u8]) -> String {
        aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A new download is downloaded once, hashed, and submitted with its
    /// URL, sha256 and note; Cactarium's answer says it awaits review.
    #[test]
    fn a_download_is_hashed_and_submitted() {
        let (base, log) = Registry { files: vec![("/id.tar.gz".into(), DATA.to_vec())], ..Default::default() }.serve();
        let url = format!("{base}/id.tar.gz");
        let said = submit_with(&Client::at(&base), "tok", &url, Some("initial data of the paper".into()))
            .unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(said.len(), 3, "{said:?}");
        assert!(said[0].starts_with(&format!("Downloaded {url}: ")) && said[0].ends_with(&format!(", sha256 {}.", sha(DATA))), "{said:?}");
        assert_eq!(said[1], format!("Submitted it to {base} for review."));
        assert_eq!(said[2], "Awaiting verification and review by Cactarium.");
        assert_eq!(paths(&log), ["/id.tar.gz", "/api/v1/downloads"]);
        let posted: DownloadSubmission = serde_json::from_str(&log.lock().unwrap()[1].1).unwrap();
        assert_eq!(
            posted,
            DownloadSubmission { url, sha256: sha(DATA), note: Some("initial data of the paper".into()) }
        );
    }

    /// A download submitted before is answered 409 with the review it has:
    /// said as such, with the server's message when it says more, and no
    /// error.
    #[test]
    fn a_download_submitted_before_says_its_review() {
        let file = |name: &str| (format!("/{name}"), DATA.to_vec());
        let registry = Registry {
            files: vec![file("p.tgz"), file("a.tgz"), file("r.tgz")],
            submitted: vec![
                ("/p.tgz".into(), "pending".into(), "the server couldn't verify it: the file's sha256 is abc, not def".into()),
                ("/a.tgz".into(), "approved".into(), "already submitted".into()),
                ("/r.tgz".into(), "rejected".into(), "already submitted".into()),
            ],
            ..Default::default()
        };
        let (base, log) = registry.serve();
        let said = |name: &str| {
            submit_with(&Client::at(&base), "tok", &format!("{base}/{name}"), None).unwrap_or_else(|e| panic!("{e:#}"))
        };
        assert_eq!(
            said("p.tgz")[1..],
            ["Already submitted, currently pending.", "the server couldn't verify it: the file's sha256 is abc, not def"]
        );
        assert_eq!(said("a.tgz")[1..], ["Already submitted, currently approved."]);
        assert_eq!(said("r.tgz")[1..], ["Already submitted, currently rejected."]);
        assert_eq!(paths(&log).iter().filter(|p| *p == "/api/v1/downloads").count(), 3);
    }

    /// A download that fails submits nothing; a Cactarium that cannot be
    /// reached fails the submission, saying so; a URL with credentials is
    /// refused before anything is asked.
    #[test]
    fn failures_submit_nothing() {
        let (base, log) = Registry::default().serve();
        // Nothing listens on port 9.
        let err = format!("{:#}", submit_with(&Client::at(&base), "tok", "http://127.0.0.1:9/f.tgz", None).unwrap_err());
        assert!(err.starts_with("Failed to download http://127.0.0.1:9/f.tgz"), "{err}");
        let err = format!("{:#}", submit_with(&Client::at(&base), "tok", &format!("{base}/missing.tgz"), None).unwrap_err());
        assert!(err.contains("404"), "{err}");
        assert!(!paths(&log).iter().any(|p| p == "/api/v1/downloads"), "{:?}", paths(&log));

        let (files, _) = Registry { files: vec![("/f.tgz".into(), DATA.to_vec())], ..Default::default() }.serve();
        let err = format!("{:#}", submit_with(&Client::at("http://127.0.0.1:9"), "tok", &format!("{files}/f.tgz"), None).unwrap_err());
        assert!(err.contains("http://127.0.0.1:9"), "{err}");

        let err = format!("{:#}", submit_with(&Client::at(&base), "tok", "https://me:s3cret@example.org/f.tgz", None).unwrap_err());
        assert!(err.starts_with("Not submitted: https://***@example.org/f.tgz carries") && !err.contains("s3cret"), "{err}");
        let err = format!("{:#}", submit_with(&Client::at(&base), "tok", &format!("{files}/f.tgz"), Some("a\nb".into())).unwrap_err());
        assert_eq!(err, "A note is one line.");
    }

    /// Ctrl-C while the file's server has gone silent mid-body fails the
    /// submission at once, with nothing submitted. The interrupt flag is
    /// the process's: run in a child of its own.
    #[test]
    fn an_interrupt_during_the_download_fails_at_once() {
        if std::env::var_os("CACTUP_TEST_SUBMIT_INTERRUPT").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["commands::download::tests::an_interrupt_during_the_download_fails_at_once", "--exact", "--test-threads=1"])
                .env("CACTUP_TEST_SUBMIT_INTERRUPT", "1")
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success() && said.contains("1 passed"), "{said}{}", String::from_utf8_lossy(&out.stderr));
            return;
        }
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (asked, asked_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.read(&mut [0u8; 4096]);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\nsome bytes").unwrap();
            asked.send(()).unwrap();
            // Then silence, holding the connection open.
            std::thread::sleep(std::time::Duration::from_secs(60));
        });
        let (base, log) = Registry::default().serve();
        let client = Client::at(&base);
        let submitting = std::thread::spawn(move || {
            let got = submit_with(&client, "tok", &format!("http://127.0.0.1:{port}/big.tgz"), None);
            (got, std::time::Instant::now())
        });
        asked_rx.recv_timeout(std::time::Duration::from_secs(10)).expect("the file was asked for");
        std::thread::sleep(std::time::Duration::from_millis(200));
        gix::interrupt::IS_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
        let interrupted = std::time::Instant::now();
        let (got, returned) = submitting.join().unwrap();
        assert_eq!(format!("{:#}", got.unwrap_err()), "Interrupted");
        let took = returned.duration_since(interrupted);
        assert!(took < std::time::Duration::from_secs(2), "{took:?}");
        assert!(paths(&log).is_empty(), "{:?}", paths(&log));
    }
}
