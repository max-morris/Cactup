//! `cactup install` — install an Einstein Toolkit release.

use super::{machine, p2s, prompt_with_default, Ctx};
use crate::args::InstallArgs;
use crate::database::CactusInstallation;
use crate::{manifest, shell, Res};
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use std::fs;
use std::path::{Path, PathBuf};

/// A `cactup install` gets its thornlist either from a release tag in the
/// manifest, or (with `--thornlist`) directly from a file — a "custom
/// installation". Everything past thornlist resolution (alias, machine,
/// prefixes, GetComponents, symlink, DB registration) is shared.
enum InstallSource<'repo> {
    Release(&'repo manifest::Tag<'repo>),
    Custom { path: PathBuf, content: String },
}

pub fn dispatch(ctx: &Ctx, args: InstallArgs) -> Res<()> {
    let InstallArgs {
        release,
        thornlist,
        alias,
        silent,
        install_prefix,
        no_symlink,
        symlink_prefix,
        symlink_name,
    } = args;

    let custom = thornlist.is_some();

    let home_dir = std::env::home_dir().ok_or(anyhow!("Failed to determine home directory"))?;
    let cactup_root = &crate::CACTUP_ROOT;

    // Custom installs need no manifest at all; validate the thornlist file up
    // front, before any prompting, so a missing/unreadable file fails fast.
    let custom_thornlist = match &thornlist {
        Some(path) => {
            let expanded = shell::expand_path(&p2s(path.clone())?);
            let content = fs::read_to_string(&expanded)
                .with_context(|| format!("Failed to read thornlist {expanded}"))?;
            // Carry the *expanded*, absolute path onward: it is what the alias
            // default, the success message, and the recorded provenance all
            // want, and a `~`-relative or cwd-relative path would be
            // meaningless once stored in the global DB.
            let expanded = PathBuf::from(&expanded);
            let resolved = fs::canonicalize(&expanded).unwrap_or(expanded);
            Some((resolved, content))
        }
        None => None,
    };

    let repo = if custom {
        None
    } else {
        Some(manifest::ensure_manifest_repo(cactup_root, &ctx.globals.manifest_url)?)
    };
    let tags = repo.as_ref().map(manifest::get_tags).transpose()?;

    // §2.3: a snapshot for the prompt-time checks; the lock is NOT held across
    // the download/build below. The final registration re-checks under the
    // lock in `ctx.db.update`.
    let database = ctx.db.read()?;

    if let Some(tags) = &tags {
        if tags.is_empty() {
            println!("No releases found.");
            return Ok(());
        }
    }

    let symlink_prefix_default = home_dir.as_path();
    let symlink_name_default = "Cactus";

    let source = match custom_thornlist {
        Some((path, content)) => InstallSource::Custom { path, content },
        None => {
            let tags = tags.as_ref().unwrap();
            let release_default = tags.first().unwrap().short_name.clone();

            let release_tag = match release {
                Some(release) => match manifest::find_tag(tags, &release) {
                    Some(tag) => tag,
                    None => {
                        println!("{}", format!("{} is not a valid release.", release.bold()).bright_red());
                        return Ok(());
                    }
                },
                None if silent => &tags[0],
                None => loop {
                    let release_sel = prompt_with_default("Which release do you want to install?", &release_default)?;
                    match manifest::find_tag(tags, &release_sel) {
                        Some(tag) => break tag,
                        None => {
                            println!("{}", format!("{} is not a valid release.", release_sel.bold()).bright_red());
                        }
                    }
                },
            };

            InstallSource::Release(release_tag)
        }
    };

    let (alias_default, alias_default_source) = match &source {
        InstallSource::Release(release_tag) => (release_tag.short_name.clone(), "the release name"),
        InstallSource::Custom { path, .. } => {
            (path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), "the thornlist file name")
        }
    };

    let alias = match alias {
        Some(alias) if database.installations.contains_key(&alias) => {
            println!("{}", format!("An installation with the alias {} already exists.", alias.bold()).bright_red());
            return Ok(());
        },
        Some(alias) if !valid_alias(&alias) => {
            println!("{}", format!("{} is not a valid alias: use only letters, digits, '-', '_', and '.'.", alias.bold()).bright_red());
            return Ok(());
        }
        Some(alias) => alias,
        None if silent && !valid_alias(&alias_default) => {
            println!("{}", format!("{} (derived from {}) is not a valid alias: use only letters, digits, '-', '_', and '.'. Please choose one by passing {}.", alias_default.bold(), alias_default_source, "--alias".bold()).bright_red());
            return Ok(());
        }
        None if silent && database.installations.contains_key(&alias_default) => {
            println!("{}", format!("An installation with the alias {} (derived from {}) already exists. Please choose a different alias by passing {}.", alias_default.bold(), alias_default_source, "--alias".bold()).bright_red());
            return Ok(());
        }
        None if silent => alias_default.clone(),
        None => loop {
            let alias_sel = prompt_with_default("What should the installation's alias be? This unique name will be used to identify the installation in the future.", &alias_default)?;
            if database.installations.contains_key(&alias_sel) {
                println!("{}", format!("An installation with the alias {} already exists. Please choose another.", alias_sel.bold()).bright_red());
            } else if !valid_alias(&alias_sel) {
                println!("{}", format!("{} is not a valid alias: use only letters, digits, '-', '_', and '.'.", alias_sel.bold()).bright_red());
            } else {
                break alias_sel;
            }
        }
    };

    // §4.7: an unrecognized host gets a machine persisted into the user MDB
    // right here — the successor to simfactory's `sim setup-silent`. Resolved
    // now (rather than after the prompts below) so the install-location default
    // can honor this machine's [paths].install-home.
    let machine = machine::ensure_local_machine(ctx)?;

    // The install-location default honors the machine's [paths].install-home
    // (§4.2) — @USER@/@ENV()@ resolved for this host — falling back to
    // ~/.cactup/cacti when the machine omits it. Installs land under
    // <install-home>/<alias>.
    let install_home_base = machine
        .meta
        .resolved_paths()
        .ok()
        .and_then(|paths| paths.install_home)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| cactup_root.join("cacti"));
    let install_prefix_default = |alias: &str| p2s(install_home_base.join(alias));

    let install_prefix = match install_prefix {
        Some(install_prefix) => install_prefix,
        None if silent => install_prefix_default(&alias)?,
        None => prompt_with_default("Where should the installation live? The Cactus directory will be created here.", &install_prefix_default(&alias)?)?
    };
    let install_prefix = shell::expand_path(&install_prefix);

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
    let symlink_prefix = shell::expand_path(&symlink_prefix);

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

    // We can seed the user knob from the machine's user in basically any case.
    {
        let snapshot = ctx.db.read()?;

        if snapshot.knob("user").is_none() && let Some(user) = snapshot.knob_or_default("user") {
            ctx.db.update(|db| {
                db.set_knob("user", user);
                Ok(())
            })?;
        }
    }

    // Prompt for knobs if they haven't been set yet.
    if !silent {
        let snapshot = ctx.db.read()?;

        let mut answers = Vec::new();
        for (knob, question) in [
            ("user", "Your username on this machine?"),
            ("email", "Your email (for job notifications)?"),
            ("allocation", "Your default allocation/account (empty for none)?"),
        ] {
            if snapshot.knob(knob).is_none() {
                let default = snapshot.knob_or_default(knob).unwrap_or_default();
                let answer = prompt_with_default(question, &default)?;
                if !answer.is_empty() {
                    answers.push((knob, answer));
                }
            }
        }
        ctx.db.update(|db| {
            for (knob, value) in &answers {
                db.set_knob(knob, value.clone());
            }
            Ok(())
        })?;
    }

    let install_dir = Path::new(&install_prefix);
    fs::create_dir_all(install_dir)
       .with_context(|| format!("Failed to create installation directory {}", install_dir.display()))?;

    // Resolve to an absolute path. A symlink stores its target verbatim, so a
    // relative `install_prefix` would otherwise yield a target interpreted
    // relative to the *link's* directory, producing a dangling/wrong symlink.
    let install_dir = fs::canonicalize(install_dir)
       .with_context(|| format!("Failed to resolve installation directory {}", install_dir.display()))?;

    let thorn_list: Vec<u8> = match &source {
        InstallSource::Release(release_tag) => {
            release_tag.read_file("einsteintoolkit.th")
                       .with_context(|| format!("Failed to read einsteintoolkit.th from release {}", release_tag.short_name))?
        }
        InstallSource::Custom { content, .. } => content.clone().into_bytes(),
    };

    // The pristine as-fetched copy (§3.2): the baseline `installation
    // refetch`'s hand-edit guard compares against.
    fs::write(install_dir.join(crate::installation::SOURCE_THORNLIST), &thorn_list).with_context(|| {
        format!("Failed to write {} to {}", crate::installation::SOURCE_THORNLIST, install_dir.display())
    })?;

    // --- Native component fetch (§3.2; GetComponents is gone) ---
    let thorn_list_text =
        String::from_utf8(thorn_list.clone()).with_context(|| "the source thornlist is not UTF-8")?;
    let list = crate::thornlist::parse(&thorn_list_text)
        .with_context(|| "Failed to parse the source thornlist")?;
    for w in list.warnings() {
        println!("{}", format!("thornlist warning: {w}").yellow());
    }
    // Phase-scoped renderer: probing every repo (a gix status walk each) can
    // take a while, and without a bar that looks like a hang before
    // anything else appears. Shut down before any subsequent stdout print —
    // the renderer draws on stderr and must not fight it.
    let (progress, renderer) = manifest::setup_prodash();
    let mut classify = progress.add_child("classify repos");
    let plan = crate::fetch::plan(&list, &install_dir, &mut classify)?;
    drop(classify);
    renderer.shutdown_and_wait();

    let report = crate::fetch::execute(&plan, &install_dir)?;
    if !report.failures.is_empty() {
        for f in &report.failures {
            println!("{}", format!("  {}: {}", f.what, f.error).bright_red());
        }
        return Err(anyhow!("component fetch failed for {} item(s)", report.failures.len()));
    }
    crate::fetch::FetchState::record(&install_dir, &report.records())?;

    // The live, editable copy, where GetComponents' COMPONENTLIST_TARGET used
    // to land it (under its own name — the directory is inherited, the filename
    // is ours); build.rs::resolve_thornlist depends on it being here.
    let live_dir = install_dir.join("Cactus").join("thornlists");
    fs::create_dir_all(&live_dir)
        .with_context(|| format!("Failed to create {}", live_dir.display()))?;
    fs::write(live_dir.join(crate::installation::LIVE_THORNLIST), &thorn_list).with_context(|| {
        format!("Failed to write {}", live_dir.join(crate::installation::LIVE_THORNLIST).display())
    })?;

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

    // Fix sim-home/test-home into installation.toml now, from the machine's
    // [paths] (§8.1, §11.5) — every later sim/test command resolves against
    // these, and they are set exactly once, at install time.
    crate::installation::Installation::new(&alias, &install_dir).ensure_meta(&machine)?;

    let release_name = match &source {
        InstallSource::Release(release_tag) => Some(release_tag.short_name.clone()),
        InstallSource::Custom { .. } => None,
    };
    // A custom installation has no release name to show, so record the
    // thornlist it came from and let `show`/`list` name that instead.
    let source_thornlist = match &source {
        InstallSource::Release(_) => None,
        InstallSource::Custom { path, .. } => Some(path.display().to_string()),
    };

    let became_active = ctx.db.update(|database| {
        if database.installations.contains_key(&alias) {
            bail!(
                "an installation named {alias} appeared while this install ran; \
                 the new installation at {} was NOT registered",
                install_dir.display()
            );
        }
        database.installations.insert(alias.clone(), CactusInstallation {
            alias: alias.clone(),
            release: release_name.clone(),
            path: install_dir.to_string_lossy().to_string(),
            thornlist: source_thornlist.clone(),
            current_release: None,
            current_thornlist: None,
        });
        if database.active_installation.is_none() {
            database.active_installation = Some(alias.clone());
            Ok(true)
        } else {
            Ok(false)
        }
    })?;

    match &source {
        InstallSource::Release(release_tag) => {
            println!("{}", format!("Success! Installed release {} into {}", release_tag.short_name, install_dir.join("Cactus").display()).bold().bright_green());
        }
        InstallSource::Custom { path, .. } => {
            println!("{}", format!("Success! Installed custom thornlist {} into {}", path.display(), install_dir.join("Cactus").display()).bold().bright_green());
        }
    }
    if do_symlink {
        println!("{}", format!("Created a symlink at {}/{}", symlink_prefix, symlink_name).bold().bright_green());
    }

    if !became_active {
        println!("Another installation is already active. To switch to this installation, run `{}`", format!("cactup use {}", alias).bold());
    }

    Ok(())
}

/// An alias must be usable verbatim as a path component.
fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias != "."
        && alias != ".."
        && alias.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::valid_alias;

    #[test]
    fn alias_validation() {
        for good in ["ET_2025_05", "et-2025.05", "mybuild", "a"] {
            assert!(valid_alias(good), "{good} should be valid");
        }
        for bad in ["", ".", "..", "has space", "slash/y", "tilde~", "a\tb"] {
            assert!(!valid_alias(bad), "{bad:?} should be invalid");
        }
    }
}
