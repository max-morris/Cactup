mod args;
mod database;
mod manifest;
mod shell;

use crate::args::{Args, Commands};
use anyhow::{anyhow, Context};
use clap::Parser;
use directories::BaseDirs;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use colored::Colorize;
use crate::database::{CactusInstallation, Database};

type Res<T> = anyhow::Result<T>;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub static CACTUP_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    let base_dirs = BaseDirs::new().expect("Failed to get base directories");
    let home_dir = base_dirs.home_dir().to_path_buf();
    home_dir.join(".cactup")
});


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

fn main() -> Res<()> {
    unsafe {
        // SAFETY: This method is unsafe because the signal handler we pass in has a certain contract.
        //         We satisfy the contract by virtue of doing nothing.
        gix::interrupt::init_handler(0, || {})?;
    }

    let args = Args::parse();

    let base_dirs = BaseDirs::new().ok_or(anyhow!("Failed to determine base directories"))?;

    let cactup_root = &CACTUP_ROOT;
    let database = Database::load()?;

    match args.command {
        Commands::List { all } => {
            let repo = manifest::ensure_manifest_repo(cactup_root, &args.manifest_url)?;

            let tags = manifest::get_tags(&repo)?;

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
        Commands::Show => {
            let database = lock!(database);

            if database.installations.is_empty() {
                println!("{}", "No installations found.".bright_red());
                return Ok(());
            }

            for installation in database.installations.values() {
                print!("- {}", installation.alias.bold());
                if let Some(release) = &installation.release {
                    print!(" (release {})", release.bold());
                } else {
                    print!(" (manual installation)");
                }
                if let Some(active_installation) = &database.active_installation && *active_installation == installation.alias {
                    print!("{}", " (active)".bold().bright_green());
                }
                println!();
                if args.verbose {
                    println!("\t Path: {}", installation.path);
                }
            }
        }
        Commands::Use { alias } => {
            let mut database = lock!(database);
            if !database.installations.contains_key(&alias) {
                println!("{}", format!("There is no installation named {}.", alias.bold()).bright_red());
                return Ok(());
            }

            println!("{}", format!("Switched to installation {}.", &alias.bold()).bright_green());
            database.active_installation = Some(alias);
        }
        Commands::Install {
            release,
            alias,
            silent,
            install_prefix,
            no_symlink,
            symlink_prefix,
            symlink_name
        } => {
            let repo = manifest::ensure_manifest_repo(cactup_root, &args.manifest_url)?;
            let tags = manifest::get_tags(&repo)?;
            let mut database = lock!(database);

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

            let alias = match alias {
                Some(alias) if database.installations.contains_key(&alias) => {
                    println!("{}", format!("An installation with the alias {} already exists.", alias.bold()).bright_red());
                    return Ok(());
                },
                Some(alias) => alias,
                None if silent && database.installations.contains_key(release) => {
                    println!("{}", format!("An installation with the alias {} (derived from the release name) already exists. Please choose a different alias by passing {}.", release.bold(), "--alias".bold()).bright_red());
                    return Ok(());
                }
                None if silent => release.to_owned(),
                None => loop {
                    let alias_sel = prompt_with_default("What should the installation's alias be? This unique name will be used to identify the installation in the future.", release)?;
                    if database.installations.contains_key(&alias_sel) {
                        println!("{}", format!("An installation with the alias {} already exists. Please choose another.", alias_sel.bold()).bright_red());
                    } else {
                        break alias_sel;
                    }
                }
            };

            let install_prefix = match install_prefix {
                Some(install_prefix) => install_prefix,
                None if silent => install_prefix_default(release)?,
                None => prompt_with_default("Where should the installation live?", &install_prefix_default(release)?)?
            };
            let install_prefix = shell::expand_path(&install_prefix, &base_dirs);

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
            let symlink_prefix = shell::expand_path(&symlink_prefix, &base_dirs);

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

            database.installations.insert(alias.clone(), CactusInstallation {
                alias: alias.clone(),
                release: Some(release.clone()),
                path: install_dir.to_string_lossy().to_string(),
            });

            println!("{}", format!("Success! Installed release {} into {}", release_tag.short_name, install_dir.join("Cactus").display()).bold().bright_green());
            if do_symlink {
                println!("{}", format!("Created a symlink at {}/{}", symlink_prefix, symlink_name).bold().bright_green());
            }

            if database.active_installation.is_none() {
                database.active_installation = Some(alias.clone());
            } else {
                println!("Another installation is already active. To switch to this installation, run `{}`", format!("cactup use {}", alias).bold());
            }
        }
    }

    Ok(())
}
