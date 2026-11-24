//! The first Ctrl-C promises "stopping": a command it lands in fails, even
//! one whose work then goes on to finish. Driven through the cactup binary.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CACTUP: &str = env!("CARGO_BIN_EXE_cactup");

/// `cactup machine list` (from the repo's own MDB) with its stdout a pipe
/// already full, so the command stops in its first write, its interrupt
/// handler long installed; with `interrupt`, a SIGINT lands there. The pipe
/// is then drained and the command finishes its work. Its exit status, the
/// bytes it wrote, and its stderr.
fn machine_list_held(interrupt: bool) -> (Option<i32>, usize, String) {
    let home = tempfile::tempdir().unwrap();
    let (mut reader, mut writer) = std::io::pipe().unwrap();
    // Full, whatever its size: written to until it would block.
    let flags = rustix::fs::fcntl_getfl(&writer).unwrap();
    rustix::fs::fcntl_setfl(&writer, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let mut filled = 0;
    loop {
        match writer.write(&[b'x'; 4096]) {
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("{e}"),
        }
    }
    rustix::fs::fcntl_setfl(&writer, flags).unwrap();
    let mut child = Command::new(CACTUP)
        .args(["--mdb-path", concat!(env!("CARGO_MANIFEST_DIR"), "/mdb"), "machine", "list"])
        .env("CACTUP_HOME", home.path())
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Held in its write: the kernel says so where it can; elsewhere, two
    // seconds is far longer than it takes to get there.
    let wchan = format!("/proc/{}/wchan", child.id());
    let started = Instant::now();
    loop {
        match std::fs::read_to_string(&wchan) {
            Ok(at) if at == "pipe_write" => break,
            Ok(at) if at == "0" || at.is_empty() => {
                if started.elapsed() > Duration::from_secs(2) {
                    break;
                }
            }
            Ok(_) => {}
            Err(e) => panic!("cactup is gone before writing its output: {e}"),
        }
        assert!(started.elapsed() < Duration::from_secs(30), "cactup never got to writing its output");
        std::thread::sleep(Duration::from_millis(20));
    }
    if interrupt {
        let pid = rustix::process::Pid::from_child(&child);
        rustix::process::kill_process(pid, rustix::process::Signal::INT).unwrap();
        // The handler runs (and the write resumes) before the pipe is drained.
        std::thread::sleep(Duration::from_millis(300));
    }
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("cactup did not finish once its output was read");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    (status.code(), out.len() - filled, err)
}

/// A Ctrl-C that lands while the command is still at work, which then goes
/// on to finish it, still fails the run: exit status 1, as for any
/// interrupted error, never a clean success after "stopping".
#[test]
fn a_command_interrupted_on_its_way_to_success_fails() {
    let (status, wrote, err) = machine_list_held(true);
    assert!(wrote > 0, "{err}");
    assert!(err.contains("interrupted, stopping"), "{err}");
    assert!(err.contains("Interrupted"), "{err}");
    assert_eq!(status, Some(1), "{err}");

    // The same run, not interrupted: a success.
    let (status, wrote, err) = machine_list_held(false);
    assert!(wrote > 0 && !err.contains("Interrupted"), "{err}");
    assert_eq!(status, Some(0), "{err}");
}
