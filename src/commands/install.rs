//! `cactup install` — install an Einstein Toolkit release.

use super::{machine, p2s, prompt_with_default, Ctx};
use crate::args::InstallArgs;
use crate::database::CactusInstallation;
use crate::installation::validate_root_dir;
use crate::{manifest, shell, Res};
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use std::fs;
use std::path::{Path, PathBuf};

/// A `cactup install` gets its thornlist either from the manifest — a release
/// tag or the tip of master — or (with `--thornlist`) directly from a file, a
/// "custom installation". Everything past thornlist resolution (alias,
/// machine, prefixes, GetComponents, symlink, DB registration) is shared.
enum InstallSource<'repo> {
    Release(manifest::Release<'repo>),
    Custom { path: PathBuf, content: String },
    /// A `.sim` file (§8.9): `content` is its thornlist with every pinned
    /// repo pinned by `!REPO_COMMIT`, so the ordinary fetch checks each out
    /// at exactly its commit and a refetch never moves it.
    SimFile { path: PathBuf, sim: Box<cactup_ecosystem_common::sim::SimFile>, content: String },
    /// A Cactarium stack release (§8.10): `content` is its thornlist with
    /// every repo the release pins pinned by `!REPO_COMMIT`; `slug` is the
    /// exact release (`owner/stack@version`, or the unpinned `@head`/`@master`).
    Stack { slug: String, content: String, contact: Option<String> },
}

pub fn dispatch(ctx: &Ctx, args: InstallArgs) -> Res<()> {
    let InstallArgs {
        release,
        thornlist,
        simfile,
        alias,
        silent,
        install_prefix,
        no_symlink,
        symlink_prefix,
        symlink_name,
        inputs_settled,
    } = args;

    // `owner/stack[@…]` names a Cactarium stack, never a manifest release
    // (those have no `/`).
    let stack_source = match release.as_deref() {
        Some(req) if req.contains('/') => Some(stack_install_source(ctx, req)?),
        _ => None,
    };
    let custom = thornlist.is_some() || simfile.is_some() || stack_source.is_some();
    let sim_source = simfile.as_deref().map(sim_install_source).transpose()?.or(stack_source);

    let home_dir = std::env::home_dir().ok_or(anyhow!("Failed to determine home directory"))?;
    let cactup_root = &crate::CACTUP_ROOT;

    // Custom installs need no manifest at all; validate the thornlist file up
    // front, before any prompting, so a missing/unreadable file fails fast.
    let custom_thornlist = match &thornlist {
        Some(path) => {
            let expanded = shell::expand_path(&p2s(path.clone())?);
            let content = fs::read_to_string(&expanded)
                .with_context(|| format!("Failed to read thornlist '{expanded}'"))?;
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
    let releases = repo.as_ref().map(manifest::get_releases).transpose()?;

    // §2.3: a snapshot for the prompt-time checks; the lock is NOT held across
    // the download/build below. The final registration re-checks under the
    // lock in `ctx.db.update`.
    let database = ctx.db.read()?;

    // A manifest with no tags still has a master to install from, so this is
    // only fatal when the selection would have to come from the tag list.
    if let Some(releases) = &releases
        && releases.is_empty()
        && release.as_deref() != Some(manifest::MASTER)
    {
        println!("No releases found.");
        return Ok(());
    }

    let symlink_prefix_default = home_dir.as_path();

    let source = match (custom_thornlist, sim_source) {
        (_, Some(source)) => source,
        (Some((path, content)), None) => InstallSource::Custom { path, content },
        (None, None) => {
            let repo = repo.as_ref().unwrap();
            let releases = releases.as_ref().unwrap();
            // The default is always the newest *release*: master is newer, but
            // it moves under you, so it is only ever installed on request.
            let release_default = releases.first().map(|r| r.name.clone());

            let selected = match release {
                Some(release) => match manifest::resolve_release(repo, releases, &release)? {
                    Some(selected) => selected,
                    None => {
                        println!("{}", invalid_release(&release));
                        return Ok(());
                    }
                },
                None if silent => releases[0].clone(),
                None => loop {
                    let release_sel = prompt_with_default(
                        // `master` is accepted here too, and this prompt is
                        // where an interactive user decides — the flag help
                        // and `cactup releases` are the only other places
                        // that say so.
                        "Which release do you want to install (or \"master\")?",
                        release_default.as_deref().unwrap_or(manifest::MASTER),
                    )?;
                    match manifest::resolve_release(repo, releases, &release_sel)? {
                        Some(selected) => break selected,
                        None => {
                            println!("{}", invalid_release(&release_sel));
                        }
                    }
                },
            };

            InstallSource::Release(selected)
        }
    };

    // Read and parse the thornlist now, before any prompting: the symlink-name
    // default and every installed path below need `!DEFINE ROOT`, and failing
    // fast on an unparseable thornlist before prompting the user is a
    // deliberate improvement over discovering it after every question has
    // already been answered.
    let thorn_list: Vec<u8> = match &source {
        InstallSource::Release(release) => {
            release.read_file("einsteintoolkit.th")
                   .with_context(|| format!("Failed to read einsteintoolkit.th from {}", release.describe()))?
        }
        InstallSource::Custom { content, .. }
        | InstallSource::SimFile { content, .. }
        | InstallSource::Stack { content, .. } => content.clone().into_bytes(),
    };
    let thorn_list_text =
        String::from_utf8(thorn_list.clone()).with_context(|| "The source thornlist is not UTF-8")?;
    let list = crate::thornlist::parse(&thorn_list_text)
        .with_context(|| "Failed to parse the source thornlist")?;
    for w in list.warnings() {
        println!("{}", format!("Thornlist warning: {w}").yellow());
    }
    let root = list.root().to_owned();
    validate_root_dir(&root)?;
    if let InstallSource::SimFile { sim, path, .. } = &source
        && sim.sim.root != root
    {
        bail!(
            "'{}' says its source tree is {}, but its thornlist's !DEFINE ROOT is {root}",
            path.display(),
            sim.sim.root
        );
    }

    let alias_default = match &source {
        InstallSource::Release(release) => release.name.clone(),
        InstallSource::Custom { path, .. } => {
            path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        }
        InstallSource::SimFile { sim, .. } => sim.sim.name.clone(),
        // `owner/stack@1.2.0` → `stack-1.2.0`, a valid alias.
        InstallSource::Stack { slug, .. } => {
            slug.rsplit('/').next().unwrap_or(slug).replace('@', "-").replace(':', "-")
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
            println!("{}", format!("The default alias {} is not valid: use only letters, digits, '-', '_', and '.'. Pass {} to choose one.", alias_default.bold(), "--alias".bold()).bright_red());
            return Ok(());
        }
        None if silent && database.installations.contains_key(&alias_default) => {
            println!("{}", format!("An installation with the default alias {} already exists. Pass {} to choose another.", alias_default.bold(), "--alias".bold()).bright_red());
            return Ok(());
        }
        None if silent => alias_default.clone(),
        None => loop {
            let alias_sel = prompt_with_default("What should the installation's alias be?", &alias_default)?;
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

    // The install-location default honors the install-home knob, else the
    // machine's [paths].install-home (§4.2) — @USER@/@ENV()@ resolved for
    // this host — falling back to ~/.cactup/cacti when neither says.
    // Installs land under <install-home>/<alias>.
    let install_home_base = machine
        .meta
        .path_for(&database, "install-home")
        .ok()
        .flatten()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::mdb::meta::fallback_home("install-home").expect("install-home has a fallback"));
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

    // The root dir's final component names the symlink by default; see
    // `default_symlink_name`.
    let symlink_name_default = default_symlink_name(&root, &alias);

    let symlink_name = match do_symlink {
        true => {
            match symlink_name {
                Some(symlink_name) => symlink_name,
                None if silent => symlink_name_default.clone(),
                None => prompt_with_default("What should the symlink be called?", &symlink_name_default)?
            }
        },
        false => "".to_owned()
    };

    // We can seed the user knob from the machine's user in basically any case.
    {
        let snapshot = ctx.db.read()?;

        if snapshot.knob("user").is_none() && let Some(user) = snapshot.knob_or_default("user") {
            ctx.db.update(|db| {
                // Re-checked under the lock: a derived default must never
                // overwrite a value set since the snapshot was taken.
                if db.knob("user").is_none() {
                    db.set_knob("user", user);
                }
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
       .with_context(|| format!("Failed to create installation directory '{}'", install_dir.display()))?;

    // Resolve to an absolute path. A symlink stores its target verbatim, so a
    // relative `install_prefix` would otherwise yield a target interpreted
    // relative to the *link's* directory, producing a dangling/wrong symlink.
    let install_dir = fs::canonicalize(install_dir)
       .with_context(|| format!("Failed to resolve installation directory '{}'", install_dir.display()))?;

    // The pristine as-fetched copy (§3.2): the baseline `installation
    // refetch`'s hand-edit guard compares against.
    fs::write(install_dir.join(crate::installation::SOURCE_THORNLIST), &thorn_list).with_context(|| {
        format!("Failed to write {} to '{}'", crate::installation::SOURCE_THORNLIST, install_dir.display())
    })?;

    // --- Native component fetch (§3.2; GetComponents is gone) ---
    // Phase-scoped renderer: probing every repo (a gix status walk each) can
    // take a while, and without a bar that looks like a hang before
    // anything else appears. Shut down before any subsequent stdout print —
    // the renderer draws on stderr and must not fight it.
    let (progress, renderer) = manifest::setup_prodash();
    let mut classify = progress.add_child("classify repos");
    let plan = crate::fetch::plan(
        &list,
        &install_dir,
        crate::fetch::DefaultBranches::Recorded,
        &mut classify,
    )?;
    drop(classify);
    renderer.shutdown_and_wait();

    // Installing over a directory that already holds repos (a re-run after an
    // interrupted install, say): one that is modified, or only partly checked
    // out, would be left as it is and the tree registered anyway, missing
    // files and all. Refuse instead, naming them.
    if !plan.skipped.is_empty() {
        let repos_dir = crate::installation::cactus_root_of(&install_dir, &root).join("repos");
        let mut message = format!(
            "{} repo(s) in '{}' have local changes or an incomplete checkout:",
            plan.skipped.len(),
            repos_dir.display()
        );
        for s in &plan.skipped {
            message.push_str(&format!("\n  {}: {}", s.repo, s.reason.describe()));
        }
        message.push_str("\nDelete them or the installation directory, then install again.");
        bail!(message);
    }

    let report = crate::fetch::execute(&plan, &install_dir)?;
    // Ctrl-C: stop here, before anything registers the tree as an
    // installation (the per-component failures are all just "interrupted").
    if gix::interrupt::is_triggered() {
        // Running the same install again picks up where this one stopped; a
        // repo cut short mid-checkout is then named, for deleting first.
        bail!(
            "Interrupted; the installation at '{}' is incomplete and was not registered \
             (run the same install again to finish it)",
            install_dir.display()
        );
    }
    if !report.failures.is_empty() {
        for f in &report.failures {
            println!("{}", format!("  {}: {}", f.what, f.error).bright_red());
        }
        return Err(anyhow!("Component fetch failed for {} item(s)", report.failures.len()));
    }
    crate::fetch::FetchState::record(&install_dir, &report.records())?;

    // The live, editable copy, where GetComponents' COMPONENTLIST_TARGET used
    // to land it (under its own name — the directory is inherited, the filename
    // is ours); build.rs::resolve_thornlist depends on it being here.
    let live_dir = crate::installation::cactus_root_of(&install_dir, &root).join("thornlists");
    fs::create_dir_all(&live_dir)
        .with_context(|| format!("Failed to create '{}'", live_dir.display()))?;
    fs::write(live_dir.join(crate::installation::LIVE_THORNLIST), &thorn_list).with_context(|| {
        format!("Failed to write '{}'", live_dir.join(crate::installation::LIVE_THORNLIST).display())
    })?;

    if do_symlink {
        fs::create_dir_all(Path::new(&symlink_prefix))
           .with_context(|| format!("Failed to create symlink prefix '{}'", symlink_prefix))?;
        let link_path = Path::new(&symlink_prefix).join(&symlink_name);
        let target = crate::installation::cactus_root_of(&install_dir, &root);

        // Replace a stale symlink from a previous run, but refuse to clobber a real file/dir.
        match fs::symlink_metadata(&link_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                #[cfg(unix)]
                fs::remove_file(&link_path)
                   .with_context(|| format!("Failed to remove existing symlink '{}'", link_path.display()))?;
                #[cfg(windows)]
                fs::remove_dir(&link_path) // dir symlinks on Windows are removed with remove_dir
                   .with_context(|| format!("Failed to remove existing symlink '{}'", link_path.display()))?;
            }
            Ok(_) => {
                return Err(anyhow!(
                    "Refusing to overwrite '{}': it is not a symlink",
                    link_path.display()
                ));
            }
            Err(_) => {} // nothing there yet
        }

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link_path)
                          .with_context(|| format!("Failed to symlink '{}' -> '{}'", link_path.display(), target.display()))?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target, &link_path)
                             .with_context(|| format!("Failed to symlink '{}' -> '{}'", link_path.display(), target.display()))?;
    }

    // Fix sim-home/test-home into installation.toml now, from the machine's
    // [paths] (§8.1, §11.5) — every later sim/test command resolves against
    // these, and they are set exactly once, at install time.
    crate::installation::Installation::new(&alias, &install_dir).ensure_meta(&machine, &database, Some(&root))?;

    let release_name = match &source {
        InstallSource::Release(release) => Some(release.name.clone()),
        InstallSource::Custom { .. } | InstallSource::SimFile { .. } | InstallSource::Stack { .. } => None,
    };
    // A custom installation has no release name to show, so record the
    // thornlist it came from and let `show`/`list` name that instead.
    let source_thornlist = match &source {
        InstallSource::Release(_) | InstallSource::SimFile { .. } | InstallSource::Stack { .. } => None,
        InstallSource::Custom { path, .. } => Some(path.display().to_string()),
    };
    let (source_stack, stack_contact) = match &source {
        InstallSource::Stack { slug, contact, .. } => (Some(slug.clone()), contact.clone()),
        _ => (None, None),
    };
    let source_simfile = match &source {
        InstallSource::SimFile { path, .. } => Some(path.display().to_string()),
        _ => None,
    };

    let became_active = ctx.db.update(|database| {
        if database.installations.contains_key(&alias) {
            bail!(
                "An installation named {alias} appeared while this install ran; \
                 the new installation at '{}' was NOT registered",
                install_dir.display()
            );
        }
        database.installations.insert(alias.clone(), CactusInstallation {
            alias: alias.clone(),
            release: release_name.clone(),
            path: install_dir.to_string_lossy().to_string(),
            thornlist: source_thornlist.clone(),
            simfile: source_simfile.clone(),
            stack: source_stack.clone(),
            stack_contact: stack_contact.clone(),
            current_release: None,
            current_thornlist: None,
            unfetched_repos: Default::default(),
        });
        if database.active_installation.is_none() {
            database.active_installation = Some(alias.clone());
            Ok(true)
        } else {
            Ok(false)
        }
    })?;

    let installed_root = crate::installation::cactus_root_of(&install_dir, &root);
    match &source {
        InstallSource::Release(release) => {
            println!("{}", format!("Success! Installed release {} into '{}'", release.describe(), installed_root.display()).bold().bright_green());
        }
        InstallSource::Custom { path, .. } => {
            println!("{}", format!("Success! Installed custom thornlist '{}' into '{}'", path.display(), installed_root.display()).bold().bright_green());
        }
        InstallSource::Stack { slug, .. } => {
            println!("{}", format!("Success! Installed stack {slug} into '{}'", installed_root.display()).bold().bright_green());
        }
        InstallSource::SimFile { path, sim, .. } => {
            println!("{}", format!("Success! Installed the sources of '{}' into '{}'", path.display(), installed_root.display()).bold().bright_green());
            if !inputs_settled {
                for input in &sim.inputs {
                    let what = input.description.as_deref().map(|d| format!(": {d}")).unwrap_or_default();
                    println!("Its parfile needs knob {}{what}", input.knob.bold());
                }
            }
        }
    }
    if do_symlink {
        println!("{}", format!("Created a symlink at '{}/{}'", symlink_prefix, symlink_name).bold().bright_green());
    }

    if !became_active {
        println!("Another installation is active; run `{}` to switch to this one.", format!("cactup use {}", alias).bold());
    }

    Ok(())
}

/// What to print for a release selector that resolved to nothing. Names
/// `master` explicitly: it is not in `cactup releases`' listing, so a user who
/// has only seen tags has no way to know it is accepted.
fn invalid_release(selector: &str) -> String {
    format!(
        "{} is not a valid release; pass one from `cactup releases`, or {}.",
        selector.bold(),
        manifest::MASTER.bold()
    ).bright_red().to_string()
}

/// The symlink name default: `root`'s final path component ("Cactus" for a
/// stock thornlist, whatever a custom `!DEFINE ROOT` names for one that
/// overrides it). `validate_root_dir` has already rejected `.`, "", and any
/// other root with no real final component, so `file_name()` always finds
/// one; falling back to `alias` is just a safety net.
fn default_symlink_name(root: &str, alias: &str) -> String {
    Path::new(root).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| alias.to_owned())
}

/// An alias must be usable verbatim as a path component.
fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias != "."
        && alias != ".."
        && alias.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Resolve `req` (`owner/stack[@range|version|head|master]`) on the
/// Cactarium (§8.10) to one release, fetched with its pins and pinned into
/// its thornlist. A range is resolved once, here: the installation records
/// the exact release, and moving to another is an explicit refetch.
fn stack_install_source(ctx: &Ctx, req: &str) -> Res<InstallSource<'static>> {
    let crate::cactarium::StackRelease { slug, text: content, unpinned, contact } =
        crate::cactarium::stack_release_text(&ctx.db.read()?, req, ctx.globals.risky_allow_untrusted_artifacts)?;
    if unpinned {
        println!(
            "{}",
            format!("{slug} is not a release: its repos follow their branches, so the installation is not reproducible")
                .yellow()
        );
    }
    Ok(InstallSource::Stack { slug, content, contact })
}

/// Read a `.sim` file for `install --simfile` (§8.9): its thornlist pinned by
/// `!REPO_COMMIT` to every commit it records and by `!CHECKSUM_SHA256` to
/// every download hash, and a word on what it does not pin — repos it leaves
/// on their branch, sources that were dirty when it was exported (their
/// edits are not in the file), a hash that names no single file.
fn sim_install_source(path: &Path) -> Res<InstallSource<'static>> {
    use cactup_ecosystem_common::sim::SimFile;
    let expanded = PathBuf::from(shell::expand_path(&p2s(path.to_owned())?));
    let text = fs::read_to_string(&expanded).with_context(|| format!("Failed to read '{}'", expanded.display()))?;
    let sim = SimFile::from_toml(&text).map_err(|e| anyhow!("'{}': {e}", expanded.display()))?;
    let pin_failed = || format!("Failed to pin the thornlist in '{}'", expanded.display());
    let (pins, unchecked) = crate::thornlist::recorded_pins(&sim.thornlist.text, &sim.repos).with_context(pin_failed)?;
    let content = crate::thornlist::pin_text(&sim.thornlist.text, &pins).with_context(pin_failed)?;
    let names = |pred: &dyn Fn(&cactup_ecosystem_common::sim::RepoPin) -> bool| -> Vec<&str> {
        sim.repos.iter().filter(|r| pred(r)).map(|r| r.name.as_str()).collect()
    };
    let unpinned = names(&|r| r.commit.is_none() && r.sha256.is_none());
    if !unpinned.is_empty() {
        println!("{}", format!("Not pinned, so they follow their branch: {}", unpinned.join(", ")).yellow());
    }
    let dirty = names(&|r| r.dirty);
    if !dirty.is_empty() {
        println!(
            "{}",
            format!("Had uncommitted changes when exported, which the file does not carry: {}", dirty.join(", "))
                .yellow()
        );
    }
    if !unchecked.is_empty() {
        println!(
            "{}",
            format!(
                "Downloaded without checking their recorded hash, which names no single file: {}",
                unchecked.join(", ")
            )
            .yellow()
        );
    }
    let resolved = fs::canonicalize(&expanded).unwrap_or(expanded);
    Ok(InstallSource::SimFile { path: resolved, sim: Box::new(sim), content })
}

#[cfg(test)]
mod tests {
    use super::{default_symlink_name, sim_install_source, valid_alias, InstallSource};
    use std::fs;

    #[test]
    fn alias_validation() {
        for good in ["ET_2025_05", "et-2025.05", "mybuild", "a"] {
            assert!(valid_alias(good), "{good} should be valid");
        }
        for bad in ["", ".", "..", "has space", "slash/y", "tilde~", "a\tb"] {
            assert!(!valid_alias(bad), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn default_symlink_name_uses_the_roots_final_component() {
        assert_eq!(default_symlink_name("Cactus", "et-dev"), "Cactus");
        assert_eq!(default_symlink_name("MyTree", "et-dev"), "MyTree");
        assert_eq!(default_symlink_name("a/b", "et-dev"), "b");
    }

    #[test]
    fn default_symlink_name_falls_back_to_the_alias_with_no_final_component() {
        // `validate_root_dir` rejects "." before this is ever called in
        // practice, but the fallback is a safety net worth pinning anyway.
        assert_eq!(default_symlink_name(".", "et-dev"), "et-dev");
    }


    /// `install --simfile` (§8.9): the `.sim`'s thornlist, pinned to its
    /// recorded commits, is what the fetch reads, so each repo lands at its
    /// pin — not the branch tip it has moved on to since.
    #[test]
    fn a_simfile_installs_each_repo_at_its_pinned_commit() {
        use cactup_ecosystem_common::sim::{Parfile, ParfileKind, RepoPin, SimFile, SimMeta, ThornlistText};
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream/carpetx");
        crate::fetch::git::testrepo::init(&upstream);
        let pinned = crate::fetch::git::testrepo::commit_tree(&upstream, &[("Makefile", "v1\n", false)]);
        crate::fetch::git::testrepo::commit_tree(&upstream, &[("Makefile", "v2\n", false)]);
        let (branch, _) = crate::fetch::git::head_of(&upstream).unwrap();
        let url = upstream.to_string_lossy().into_owned();
        let sim = SimFile {
            format: cactup_ecosystem_common::SIM_FORMAT,
            sim: SimMeta {
                name: "bhns".into(),
                summary: None,
                description: None,
                authors: vec![],
                license: None,
                contact: None,
                cactarium: None,
                root: "Cactus".into(),
            },
            thornlist: ThornlistText {
                filename: "bhns.th".into(),
                text: format!(
                    "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n!TARGET = $ROOT\n!TYPE = git\n!URL = {url}\n\
                     !REPO_BRANCH = {branch}\n!NAME = carpetx\n!CHECKOUT = Makefile\n"
                ),
                untracked: vec![],
            },
            parfile: Parfile { filename: "bhns.par".into(), kind: ParfileKind::Par, vars: vec![], text: "x\n".into() },
            inputs: vec![],
            repos: vec![RepoPin {
                name: "carpetx".into(),
                url: url.clone(),
                git_ref: Some(branch.clone()),
                commit: Some(pinned.to_string()),
                sha256: None,
                cactarium: None,
                dirty: false,
            }],
            run: None,
            provenance: None,
        };
        let file = tmp.path().join("bhns.sim");
        fs::write(&file, sim.to_toml().unwrap()).unwrap();

        let InstallSource::SimFile { content, .. } = sim_install_source(&file).unwrap() else { panic!("a .sim source") };
        let list = crate::thornlist::parse(&content).unwrap();
        assert_eq!(list.components()[0].commit.as_deref(), Some(pinned.to_string().as_str()));
        let root = tmp.path().join("install");
        let mut progress = prodash::tree::Root::new().add_child("test plan");
        let plan = crate::fetch::plan(&list, &root, crate::fetch::DefaultBranches::Recorded, &mut progress)
            .unwrap();
        let report = crate::fetch::execute(&plan, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        let repo = root.join("Cactus/repos/carpetx");
        assert_eq!(crate::fetch::git::head_of(&repo).unwrap().1, pinned);
        assert_eq!(fs::read_to_string(repo.join("Makefile")).unwrap(), "v1\n");
    }
}
