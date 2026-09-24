//! `cactup update [--check] [--prune]` (§17): install the newest published
//! build, then force a machine-database sync. The binary goes first: a new
//! build may read a newer MDB generation, so after installing one this
//! re-runs itself in it and the sync happens there.

use crate::build_info::{self, Stamp};
use crate::commands::Ctx;
use crate::update::{self, Applied, Decision, Installability};
use crate::Res;
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use std::ffi::OsString;

pub fn dispatch(ctx: &Ctx, check: bool, prune: bool) -> Res<()> {
    let Some(me) = build_info::DIST.filter(|_| build_info::is_dist()) else {
        bail!(
            "this is a development build of cactup, which never updates itself; use git pull and cargo build"
        );
    };
    let base = update::update_url(&ctx.db.read()?);
    if check {
        return report(&me, &base);
    }

    // In a process an update just started, the old build already did this.
    // Any failure here waits for the machine database sync below: that is
    // worth doing whatever happened to the binary.
    let binary = if update::just_updated() { Ok(()) } else { update_binary(&me, &base) };

    if ctx.globals.mdb_path.is_none() {
        update::force_mdb_sync(ctx)?;
        if let Some(notice) = update::mdb_generation_notice() {
            eprintln!("\n{}\n", notice.yellow().bold());
        }
    }

    if prune {
        let bin_dir = crate::CACTUP_ROOT.join("bin");
        let exe = std::env::current_exe().context("Failed to locate the running cactup")?;
        let removed = update::prune_binaries(&bin_dir, &exe)?;
        for path in &removed {
            println!("removed {}", path.display());
        }
        if removed.is_empty() {
            println!("no build was retired more than 30 days ago");
        }
    }
    binary
}

/// `--check`: what is installed, what is published, and what `cactup
/// update` would do about it. Changes nothing.
fn report(me: &Stamp, base: &str) -> Res<()> {
    let latest = update::check(base).context("Could not check for a newer cactup")?;
    println!(
        "this cactup  {}  {}  machine database generation {}  ({})",
        me.id,
        update::short_date(me.date),
        build_info::MDB_GENERATION,
        build_info::TARGET
    );
    println!(
        "published    {}  {}  machine database generation {}  ({base})",
        latest.build,
        update::short_date(&latest.date),
        latest.mdb_generation
    );
    match update::decide(&latest, me, build_info::TARGET)? {
        Decision::UpToDate => println!("{}", "cactup is up to date".green()),
        Decision::ServerOlder => {
            println!("the published build is not newer than this one; nothing to install")
        }
        Decision::NoTarget => println!(
            "{}",
            format!("no build for {} is published at {base}", build_info::TARGET).yellow()
        ),
        Decision::Newer(_) => {
            println!(
                "{}",
                format!("cactup {} is available; run `cactup update`", latest.build).yellow()
            );
            let exe = std::env::current_exe().context("Failed to locate the running cactup")?;
            if let Installability::NotUpdatable(reason) =
                update::installability(&exe, &crate::CACTUP_ROOT.join("bin"))
            {
                println!("{}", format!("but this cactup cannot replace itself: {reason}").yellow());
            }
        }
    }
    if let Some(notice) = update::mdb_generation_notice() {
        eprintln!("\n{}\n", notice.yellow().bold());
    }
    Ok(())
}

/// Install the published build if it is newer, and on success continue as
/// `cactup update` in it. Returns only when there was nothing to install,
/// nothing could be, or starting the new build failed.
fn update_binary(me: &Stamp, base: &str) -> Res<()> {
    let latest = update::check(base).context("Could not check for a newer cactup")?;
    let entry = match update::decide(&latest, me, build_info::TARGET)? {
        Decision::UpToDate => {
            println!("cactup {} ({}) is up to date", me.id, update::short_date(me.date));
            return Ok(());
        }
        Decision::ServerOlder => {
            println!(
                "cactup {} ({}) is not older than the published {} ({}); nothing to install",
                me.id,
                update::short_date(me.date),
                latest.build,
                update::short_date(&latest.date)
            );
            return Ok(());
        }
        Decision::NoTarget => {
            eprintln!(
                "{}",
                format!(
                    "cactup {} is published, but not for {}; nothing to install",
                    latest.build,
                    build_info::TARGET
                )
                .yellow()
            );
            return Ok(());
        }
        Decision::Newer(entry) => entry,
    };

    let bin_dir = crate::CACTUP_ROOT.join("bin");
    let exe = std::env::current_exe().context("Failed to locate the running cactup")?;
    if let Installability::NotUpdatable(reason) = update::installability(&exe, &bin_dir) {
        bail!("cactup {} is available, but this cactup cannot replace itself: {reason}", latest.build);
    }
    let path = match update::apply(base, &latest, &entry, &bin_dir, me)? {
        Applied::Installed(path) => path,
        Applied::AlreadyInstalled => bin_dir.join(format!("cactup-{}", latest.build)),
        Applied::Busy => {
            eprintln!(
                "{}",
                "another cactup is installing an update right now; run `cactup update` again in a moment"
                    .yellow()
            );
            return Ok(());
        }
        Applied::NotYet => {
            eprintln!(
                "{}",
                format!(
                    "cactup {} is announced, but its download is not available yet (a release is still \
                     propagating); try again in a few minutes",
                    latest.build
                )
                .yellow()
            );
            return Ok(());
        }
        Applied::NotUpdatable(reason) => {
            bail!("cactup {} is available, but cannot be installed: {reason}", latest.build)
        }
    };
    // The same command line, so global flags (-K, --mdb-path) carry over.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let e = update::exec_updated(&path, &args);
    Err(anyhow!(e).context(format!("Failed to start the updated cactup at {}", path.display())))
}
