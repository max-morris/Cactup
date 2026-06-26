use std::collections::HashMap;
use anyhow::{anyhow, Context};
use serde_derive::{Deserialize, Serialize};
use signal_hook::consts::TERM_SIGNALS;
use signal_hook::iterator::Signals;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::Res;

/// Lock an `Arc<Mutex<Database>>` (or any `Mutex`), recovering from poisoning.
///
/// A poisoned mutex means a thread panicked while holding the lock. For our use case, we
/// just ignore the panic and recover what was there anyway.
#[macro_export]
macro_rules! lock {
    ($db:expr) => {
        $db.lock().unwrap_or_else(|e| e.into_inner())
    };
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CactusInstallation {
    pub alias: String,
    pub release: Option<String>,
    pub path: String
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Database {
    pub cactup_version: String,
    pub installations: HashMap<String, CactusInstallation>, // maps alias to installation
    pub active_installation: Option<String>, // default installation to use for most commands

    // Runtime-only state. These are populated by `load` and never serialized
    // into the on-disk JSON.
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    lock: Option<File>,
}

impl Database {
    pub fn new() -> Self {
        Self {
            cactup_version: crate::VERSION.to_owned(),
            installations: HashMap::new(),
            active_installation: None,
            path: PathBuf::new(),
            lock: None,
        }
    }

    /// Read the database from `~/.cactup/database.json`, creating a fresh one
    /// (in memory) if the file doesn't exist yet. The returned handle holds an
    /// exclusive advisory lock on `~/.cactup/database.lock` for its whole
    /// lifetime, so a second cactup instance will fail fast rather than race on
    /// the same file.
    ///
    /// The database is flushed back to disk when the handle is dropped (normal
    /// exit, or error unwinding). Because a termination signal terminates the
    /// process without running destructors, `load` also spawns a thread that
    /// makes a best-effort attempt to persist the database when the program is
    /// killed (SIGINT, SIGTERM, ...). See [`Database::install_kill_handler`].
    pub fn load() -> Res<Arc<Mutex<Self>>> {
        let dir = &crate::CACTUP_ROOT;
        fs::create_dir_all(dir.as_path())
            .with_context(|| format!("Failed to create cactup directory {}", dir.display()))?;

        // Acquire an exclusive, non-blocking lock on a dedicated lock file. We
        // hold this handle (in `self.lock`) until the Database is dropped, at
        // which point the OS releases the lock automatically. Running multiple
        // cactup instances at once is discouraged; this turns a silent
        // last-writer-wins clobber into an explicit error.
        let lock_path = dir.join("database.lock");
        let lock = 
            OpenOptions::new()
                        .create(true)
                        .read(true)
                        .write(true)
                        .truncate(false)
                        .open(&lock_path)
                        .with_context(|| format!("Failed to open lock file {}", lock_path.display()))?;

        lock.try_lock().map_err(|e| {
            anyhow!(
                "Could not lock {} ({e}). Is another cactup instance running? \
                 Running multiple instances at once is not supported.",
                lock_path.display()
            )
        })?;

        let path = dir.join("database.json");
        let mut db = Self::read_from(&path)?;

        db.path = path;
        db.lock = Some(lock);

        let db = Arc::new(Mutex::new(db));
        Self::install_kill_handler(&db);
        Ok(db)
    }

    /// Spawn a background thread that attempts to persist the database when the
    /// process receives a termination signal.
    ///
    /// `Drop` covers normal exit and error unwinding, but a signal kills the
    /// process outright without running destructors, so the on-disk database
    /// would be left stale. This handler persists whatever is in memory at the
    /// time the signal arrives, then terminates the process itself.
    ///
    /// Registering a handler suppresses the default "terminate" action for these
    /// signals, so we must re-emulate it once we've saved — otherwise the
    /// process would become unkillable by SIGINT/SIGTERM. We do this ourselves
    /// rather than relying on gix's interrupt handler, which is only installed
    /// for the subcommands that touch the manifest repository; this path has to
    /// work whether or not gix was activated.
    ///
    /// The thread holds only a [`Weak`] reference, so it never keeps the
    /// database alive past normal exit (which would suppress the `Drop`-based
    /// persist).
    fn install_kill_handler(db: &Arc<Mutex<Self>>) {
        let weak = Arc::downgrade(db);

        let mut signals = match Signals::new(TERM_SIGNALS) {
            Ok(signals) => signals,
            Err(e) => {
                eprintln!("Warning: could not install signal handler to persist the database on exit: {e:#}");
                return;
            }
        };

        thread::spawn(move || {
            // We only ever handle the first signal: the body terminates the
            // process, so there's nothing to loop for. `forever()` blocks until
            // one arrives. If the iterator is somehow exhausted, the thread just
            // ends, leaving the (now non-existent) signals to the default action.
            let Some(signal) = signals.forever().next() else { return };

            // The database may already be dropped (the program is exiting
            // normally and has persisted itself); if so there's nothing to
            // save, but we still honour the signal below.
            if let Some(db) = weak.upgrade() {
                let db = lock!(db);

                // We're about to overwrite database.json with state that may be
                // inconsistent (the program could have been mid-mutation). Back
                // up the current file first so a good copy can be recovered.
                if let Err(e) = db.backup() {
                    eprintln!("Warning: failed to back up database before signal persist: {e:#}");
                }

                if let Err(e) = db.persist() {
                    eprintln!("Warning: failed to persist database after signal: {e:#}");
                }
            }

            // Now actually terminate. Registering the handler above suppressed
            // the default terminate action, so re-emulate it; this works
            // regardless of whether gix installed its own handler. The explicit
            // exit is a fallback in case the signal can't be emulated (e.g. it
            // isn't recognised).
            let _ = signal_hook::low_level::emulate_default_handler(signal);
            std::process::exit(128 + signal);
        });
    }

    /// Deserialize the database from `path`, or build a fresh one if the file
    /// doesn't exist.
    fn read_from(path: &Path) -> Res<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }

        let contents = 
            fs::read_to_string(path)
               .with_context(|| format!("Failed to read database from {}", path.display()))?;
        serde_json::from_str(&contents)
                   .with_context(|| format!("Failed to parse database at {}", path.display()))
    }

    /// Best-effort copy of the current on-disk `database.json` to a numbered
    /// backup (`database.json.bak0`, `database.json.bak1`, ...) before it gets
    /// overwritten. Used by the kill handler, whose flushed state may be
    /// inconsistent; the backup preserves the last cleanly-written file for
    /// manual recovery.
    ///
    /// The smallest unused number is chosen, so existing backups are never
    /// clobbered. Does nothing if there's no file on disk yet.
    fn backup(&self) -> Res<()> {
        if !self.path.exists() {
            return Ok(());
        }

        let mut n: u32 = 0;
        let backup_path = loop {
            let mut candidate = self.path.clone().into_os_string();
            candidate.push(format!(".bak{n}"));
            let candidate = PathBuf::from(candidate);
            if !candidate.exists() {
                break candidate;
            }
            n += 1;
        };

        fs::copy(&self.path, &backup_path).with_context(|| {
            format!(
                "Failed to back up {} to {}",
                self.path.display(),
                backup_path.display()
            )
        })?;
        Ok(())
    }

    /// Write the current state back to `self.path` as pretty-printed JSON.
    fn persist(&self) -> Res<()> {
        let contents =
            serde_json::to_string_pretty(self)
                       .with_context(|| "Failed to serialize database")?;
        fs::write(&self.path, contents)
           .with_context(|| format!("Failed to write database to {}", self.path.display()))?;
        Ok(())
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // A Database built via `new()` (rather than `load()`) has no backing
        // path and was never locked; there's nothing to persist.
        if self.path.as_os_str().is_empty() {
            return;
        }

        if let Err(e) = self.persist() {
            // Drop can't return an error, so the best we can do is warn.
            eprintln!("Warning: failed to persist database: {e:#}");
        }

        // The lock is released when `self.lock`'s File handle is dropped along
        // with the rest of `self`.
    }
}
