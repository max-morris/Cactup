mod args;

use crate::args::{Args, Commands};
use anyhow::{anyhow, Context};
use clap::Parser;
use directories::BaseDirs;
use prodash::{Progress, Root};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use colored::Colorize;
use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
use gix::remote::Direction;
use prodash::render::line::{JoinHandle, StreamKind};
use gix::remote::fetch::Tags;
use gix::{Object, Reference, Repository};
use gix::date::Time;

type Res<T> = anyhow::Result<T>;
type ProgressHandle = Arc<prodash::tree::Root>;



fn setup_prodash() -> (ProgressHandle, JoinHandle) {
    let progress = prodash::tree::Root::new();

    let progress_renderer_options = prodash::render::line::Options {
        frames_per_second: 6.0,
        hide_cursor: true, // signal-hook feature restores cursor on exit
        ..Default::default()
    }.auto_configure(StreamKind::Stderr);

    let progress_renderer = prodash::render::line::render(
        std::io::stderr(),
        progress.downgrade(),
        progress_renderer_options
    );

    (progress, progress_renderer)
}

fn ensure_manifest_repo(cactup_root: &Path, manifest_url: &str, progress: &ProgressHandle) -> Res<Repository> {
    let manifest_dir = cactup_root.join("manifest");
    let mut fetch_progress = progress.add_child("Fetching manifest");

    //fetch_progress.info(format!("Manifest directory: {}", manifest_dir.display()));

    if manifest_dir.exists() && !manifest_dir.is_dir() {
        return Err(anyhow!("Manifest directory exists but is not a directory"));
    }

    let need_clone = !manifest_dir.exists()
        || manifest_dir.read_dir()
                       .map(|mut d| d.next().is_none()) // Checking for empty dir
                       .unwrap_or(true);

    if need_clone {
        fetch_progress.info("Fetching the manifest for the first time.".to_string());

        fs::create_dir_all(&manifest_dir)
           .with_context(|| "Failed to create manifest directory")?;

        let mut prepare =
            gix::prepare_clone(manifest_url, &manifest_dir)?
                .configure_remote(|r| {
                    Ok(r.with_fetch_tags(Tags::All))
                });

        let (repo, _) = prepare.fetch_only(
            fetch_progress,
            &gix::interrupt::IS_INTERRUPTED
        ).with_context(|| "Failed to fetch manifest")?;

        Ok(repo)
    } else {
        fetch_progress.info("Checking for updates.".to_string());

        let fetch_progress_1 = fetch_progress.add_child("Checking for updates");
        let fetch_progress_2 = fetch_progress.add_child("Fetching updates");

        let repo =
            gix::open(&manifest_dir)
                .with_context(|| "Failed to open manifest repository")?;

        // Fetching tags won't delete old ones. To keep it simple, we'll just annihilate
        // whatever tags are already there before fetching.

        let tag_deletions: Vec<RefEdit> =
            repo.references()?
                .tags()?
                .filter_map(|t| t.ok())
                .map(|t| RefEdit {
                    change: Change::Delete {
                        expected: PreviousValue::Any,
                        log: RefLog::AndReference
                    },
                    name: t.name().to_owned(),
                    deref: false,
                })
                .collect();

        repo.edit_references(tag_deletions)?;

        let remote =
            repo.find_fetch_remote(None)? // origin
                .with_fetch_tags(Tags::All);

        remote.connect(Direction::Fetch)?
              .prepare_fetch(fetch_progress_1, Default::default())?
              .receive(fetch_progress_2, &gix::interrupt::IS_INTERRUPTED)?;

        fetch_progress.done("Manifest is up to date.".to_string());
        Ok(repo)
    }
}

struct Tag<'repo> {
    repo: &'repo Repository,
    short_name: String,
    ancestry_rank: usize,
    tree_id: gix::ObjectId
}

fn get_tags(repo: &Repository) -> Res<Vec<Tag<'_>>> {
    let mut tags: Vec<Tag<'_>> =
        repo.references()?
            .tags()?
            .filter_map(|t| t.ok())
            .filter_map(|t| Tag::new(repo, t).ok())
            .collect();

    tags.sort_by_key(|t| std::cmp::Reverse(t.ancestry_rank));
    Ok(tags)
}

impl<'repo> Tag<'repo> {
    pub fn new(repo: &'repo Repository, tag: Reference<'repo>) -> Res<Self> {
        let short_name = tag.name().shorten().to_string();
        let peeled_id = tag.into_fully_peeled_id()?;
        let tree_id = peeled_id.object()?.peel_to_commit()?.tree_id()?.detach();
        let commit_id = peeled_id.detach();

        // Number of commits between the tag and the root of the repository.
        // We use this as a stand-in for commit time to determine the release order of the tags,
        // since the git history has become too mangled for the former to work.
        // I also do not trust that the current naming convention, where the release date is
        // encoded in the tag name, will be followed in perpetuity. This method is more robust.
        let ancestry_rank = repo.rev_walk(Some(commit_id)).all()?.count();

        Ok(Self {
            repo,
            short_name,
            ancestry_rank,
            tree_id
        })
    }

    fn read_file(&self, path: impl AsRef<std::path::Path>) -> Res<Vec<u8>> {
        let tree = self.repo.find_tree(self.tree_id)?;
        Ok(
            tree.lookup_entry_by_path(&path)?
                .ok_or({
                    let path_name = path.as_ref().to_str().ok_or(anyhow!("Invalid path"))?;
                    anyhow!("File {} not found", path_name)
                })?
                .object()?
                .into_blob()
                .data
                .clone()
        )
    }
}

fn prompt_with_default(question: &str, default: &str) -> Res<String> {
    use std::io::{self, Write};

    print!("{question} (default: {}): ", default.bold());
    io::stdout().flush()?;

    let mut input = String::new();
    let n = io::stdin().read_line(&mut input)?;

    let input = input.trim();
    Ok(if n == 0 || input.is_empty() {
        default.to_owned() // Empty line or EOF accepts the default
    } else {
        input.to_owned()
    })
}

fn p2s(pb: PathBuf) -> Res<String> {
    pb.to_str()
      .map(|s| s.to_owned())
      .ok_or(anyhow!("Failed to convert path to string"))
}

/// Expand `$VAR`/`${VAR}` references against the current environment.
/// Undefined variables expand to the empty string, matching shell behaviour.
fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }

        match chars.peek() {
            // ${NAME}
            Some('{') => {
                chars.next(); // consume '{'
                let mut name = String::new();
                let mut closed = false;
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc == '}' {
                        closed = true;
                        break;
                    }
                    name.push(nc);
                }
                if closed {
                    out.push_str(&std::env::var(&name).unwrap_or_default());
                } else {
                    // Unterminated `${...` — leave it untouched.
                    out.push_str("${");
                    out.push_str(&name);
                }
            }
            // $NAME (alphanumeric/underscore, not starting with a digit)
            Some(&c2) if c2 == '_' || c2.is_ascii_alphabetic() => {
                let mut name = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc == '_' || nc.is_ascii_alphanumeric() {
                        name.push(nc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(&std::env::var(&name).unwrap_or_default());
            }
            // A lone `$` (or `$` followed by punctuation) is emitted literally.
            _ => out.push('$'),
        }
    }

    out
}

/// Expand a user-supplied path string the way a shell would: a leading `~`
/// becomes the home directory and `$VAR`/`${VAR}` are substituted. Paths typed
/// at our prompts are read straight from stdin with no shell involved, so
/// without this a literal `~` directory would be created in the current
/// working directory. Values passed as flags are already shell-expanded, so
/// running them through this again is a harmless no-op.
fn expand_path(input: &str, base_dirs: &BaseDirs) -> String {
    let expanded = expand_env_vars(input);
    let home = base_dirs.home_dir();

    if expanded == "~" {
        home.to_string_lossy().into_owned()
    } else if let Some(rest) = expanded.strip_prefix("~/") {
        home.join(rest).to_string_lossy().into_owned()
    } else {
        expanded
    }
}

fn main() -> Res<()> {
    // First Ctrl+C sets IS_INTERRUPTED so the fetch aborts gracefully; a second one force-quits.
    unsafe {
        // SAFETY: This method is unsafe because the signal handler we pass in has a certain contract.
        //         We satisfy the contract by virtue of doing nothing.
        gix::interrupt::init_handler(1, || {})?;
    }

    let args = Args::parse();

    let (progress, progress_renderer) = setup_prodash();

    let base_dirs = BaseDirs::new().ok_or(anyhow!("Failed to determine base directories"))?;

    // All cactup state lives under ~/.cactup (cactup-init installs the binary into ~/.cactup/bin).
    let cactup_root = base_dirs.home_dir().join(".cactup");

    let repo = ensure_manifest_repo(&cactup_root, &args.manifest_url, &progress)?;
    progress_renderer.shutdown_and_wait();

    match args.command {
        Commands::List { all } => {
            //progress_renderer.shutdown_and_wait();

            let tags = get_tags(&repo)?;

            if tags.is_empty() {
                println!("No releases found.");
                return Ok(());
            }

            let mut tags = tags.into_iter();

            if all {
                println!("{} {}", tags.next().unwrap().short_name.bold(), "(latest)".bold().bright_green());
                for tag in tags {
                    println!("{}", tag.short_name)
                }
            } else {
                const MAX_TAGS: usize = 10;
                println!("Showing the {MAX_TAGS} most recent releases. Pass {} to see them all.", "--all".bold());

                println!("{} {}", tags.next().unwrap().short_name.bold(), "(latest)".bold().bright_green());

                for tag in tags.take(MAX_TAGS - 1) {
                    println!("{}", tag.short_name)
                }
            }
        }
        Commands::Install {
            release,
            silent,
            install_prefix,
            no_symlink,
            symlink_prefix,
            symlink_name
        } => {
            let tags = get_tags(&repo)?;

            if tags.is_empty() {
                println!("No releases found.");
                return Ok(());
            }

            let release_default = tags.first().unwrap().short_name.clone();
            let install_prefix_default = |tag_name: &str|
                p2s(cactup_root.join("cacti")
                               .join(tag_name));
            let symlink_prefix_default = base_dirs.home_dir();
            let symlink_name_default = "Cactus";

            let release_tag = match release {
                Some(release) => {
                    match tags.iter().position(|tag| tag.short_name == release) {
                        Some(pos) => &tags[pos],
                        None => {
                            println!("{}", format!("{} is not a valid release.", release.bold()).bright_red());
                            return Ok(());
                        }
                    }
                }
                None if silent => &tags[0],
                None => loop {
                    let release_sel = prompt_with_default("Which release do you want to install?", &release_default)?;
                    match tags.iter().position(|tag| tag.short_name == release_sel) {
                        Some(pos) => break &tags[pos],
                        None => {
                            println!("{}", format!("{} is not a valid release.", release_sel.bold()).bright_red());
                        }
                    }
                }
            };

            let release = &release_tag.short_name;

            let install_prefix = match install_prefix {
                Some(install_prefix) => install_prefix,
                None if silent => install_prefix_default(release)?,
                None => prompt_with_default("Where should the installation live?", &install_prefix_default(release)?)?
            };
            let install_prefix = expand_path(&install_prefix, &base_dirs);

            let do_symlink = match no_symlink {
                true => false,
                false if silent => true,
                false => prompt_with_default("Do you want to create a symlink?", "yes")?.eq_ignore_ascii_case("yes")
            };

            let symlink_prefix = match do_symlink {
                true => {
                    match symlink_prefix {
                        Some(symlink_prefix) => symlink_prefix,
                        None if silent => p2s(symlink_prefix_default.to_owned())?,
                        None => prompt_with_default("Where should the symlink live?", &p2s(symlink_prefix_default.to_owned())?)?
                    }
                },
                false => "".to_owned()
            };
            let symlink_prefix = expand_path(&symlink_prefix, &base_dirs);

            let symlink_name = match do_symlink {
                true => {
                    match symlink_name {
                        Some(symlink_name) => symlink_name,
                        None if silent => symlink_name_default.to_owned(),
                        None => prompt_with_default("What should the symlink be called?", symlink_name_default)?
                    }
                },
                false => "".to_owned()
            };

            //let install_progress = progress.add_child("Installing");

            let install_dir = Path::new(&install_prefix);
            fs::create_dir_all(install_dir)
               .with_context(|| format!("Failed to create installation directory {}", install_dir.display()))?;

            // Resolve to an absolute path. A symlink stores its target verbatim, so a
            // relative `install_prefix` would otherwise yield a target interpreted
            // relative to the *link's* directory, producing a dangling/wrong symlink.
            let install_dir = fs::canonicalize(install_dir)
               .with_context(|| format!("Failed to resolve installation directory {}", install_dir.display()))?;

            let thorn_list =
                release_tag.read_file("einsteintoolkit.th")
                           .with_context(|| format!("Failed to read einsteintoolkit.th from release {}", release_tag.short_name))?;

            fs::write(install_dir.join("einsteintoolkit.th"), &thorn_list)
               .with_context(|| format!("Failed to write einsteintoolkit.th to {}", install_dir.display()))?;

            // --- Fetch GetComponents ---
            const GET_COMPONENTS_URL: &str =
                "https://raw.githubusercontent.com/gridaphobe/CRL/ET_2025_05/GetComponents";

            let script_bytes =
                reqwest::blocking::get(GET_COMPONENTS_URL)
                                  .and_then(|r| r.error_for_status())   // turn 404/5xx into an error
                                  .with_context(|| format!("Failed to download {GET_COMPONENTS_URL}"))?
                                  .bytes()
                                  .with_context(|| "Failed to read GetComponents response body")?;

            let script_path = install_dir.join("GetComponents");
            fs::write(&script_path, &script_bytes)
               .with_context(|| format!("Failed to write {}", script_path.display()))?;

            // --- Make it executable (Unix only; no-op concept on Windows) ---
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&script_path)?.permissions();
                perms.set_mode(0o755); // rwxr-xr-x
                fs::set_permissions(&script_path, perms)
                   .with_context(|| format!("Failed to chmod {}", script_path.display()))?;
            }

            // --- Stop the progress renderer before handing the terminal to the child ---
            //progress_renderer.shutdown_and_wait();

            // --- Run it on the user's behalf ---
            let status =
                std::process::Command::new(&script_path)
                                      .current_dir(&install_dir)
                                      .arg("einsteintoolkit.th")
                                      .status()
                                      .with_context(|| format!("Failed to execute {}", script_path.display()))?;

            if !status.success() {
                return Err(anyhow!("GetComponents exited unsuccessfully: {status}"));
            }

            // Execute setup-silent
            let sim_path = install_dir.join("Cactus/simfactory/bin/sim");
            let status =
                std::process::Command::new(&sim_path)
                                      .arg("setup-silent")
                                      .current_dir(&install_dir)
                                      .status()
                                      .with_context(|| format!("Failed to execute {}", sim_path.display()))?;

            if !status.success() {
                return Err(anyhow!("sim setup-silent exited unsuccessfully: {status}"));
            }

            if do_symlink {
                fs::create_dir_all(Path::new(&symlink_prefix))
                   .with_context(|| format!("Failed to create symlink prefix {}", symlink_prefix))?;
                let link_path = Path::new(&symlink_prefix).join(&symlink_name);
                let target = install_dir.join("Cactus");

                // Replace a stale symlink from a previous run, but refuse to clobber a real file/dir.
                match fs::symlink_metadata(&link_path) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        #[cfg(unix)]
                        fs::remove_file(&link_path)
                           .with_context(|| format!("Failed to remove existing symlink {}", link_path.display()))?;
                        #[cfg(windows)]
                        fs::remove_dir(&link_path) // dir symlinks on Windows are removed with remove_dir
                           .with_context(|| format!("Failed to remove existing symlink {}", link_path.display()))?;
                    }
                    Ok(_) => {
                        return Err(anyhow!(
                            "Refusing to overwrite existing path {} (not a symlink)",
                            link_path.display()
                        ));
                    }
                    Err(_) => {} // nothing there yet
                }

                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &link_path)
                                  .with_context(|| format!("Failed to symlink {} -> {}", link_path.display(), target.display()))?;
                #[cfg(windows)]
                std::os::windows::fs::symlink_dir(&target, &link_path)
                                     .with_context(|| format!("Failed to symlink {} -> {}", link_path.display(), target.display()))?;
            }

            println!("{}", format!("Success! Installed release {} into {}", release_tag.short_name, install_dir.join("Cactus").display()).bold().bright_green());
            if do_symlink {
                println!("{}", format!("Created a symlink at {}/{}", symlink_prefix, symlink_name).bold().bright_green());
            }
        }
    }

    Ok(())
}
