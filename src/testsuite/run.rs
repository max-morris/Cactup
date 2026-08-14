//! `test run` / `test submit` (§11.6): the simplified one-shot path — no
//! chaining, no recovery, no restart bookkeeping. Drives the flesh testsuite
//! (`make <config>-testsuite`) through the machine's TEST runscript variant
//! and records the pass/fail summary.

use crate::args::TestStartArgs;
use crate::build::{self, ConfigMeta};
use crate::commands::{machine, Ctx};
use crate::database::{Database, SCHEMA};
use crate::installation::{Installation, TestEntry};
use crate::lock::LinkLock;
use crate::mdb::{Machine, Phase, ScriptKind, Universe, HOST_UNIVERSE};
use crate::scheduler::{JobStatus, Scheduler};
use crate::sim::restart::{freeze_vars, thaw_vars, UniverseSpec, NO_JOB_ID};
use crate::sim::start::{
    generate_script, resolve_run_universe, resolve_submit_universe, script_command,
    spawn_and_wait, write_executable, Identity,
};
use crate::sim::vars::{
    apply_tasks_default, default_checkpt_buffer, resolve_topology, set_machine_vars,
    set_topology_vars, set_walltime_vars, Topology,
};
use crate::template::VarSet;
use crate::testsuite::{
    activate_results, active_results_id, next_results_id, results_dir, results_name, test_run_id,
    ResultsSummary, TestMeta, TestRun, Timestamps,
};
use crate::Res;
use anyhow::{anyhow, bail, Context};
use chrono::Utc;
use colored::Colorize;
use regex::Regex;
use std::fs;
use std::path::Path;

/// Default task count for testsuite runs when neither the command line nor
/// the script variant's `tasks` setting says otherwise (§11.6): thorn tests
/// are written for 1–2 MPI ranks, so filling the node (§8.5) breaks them.
const TESTSUITE_DEFAULT_TASKS: u32 = 2;

/// Entry point for both `test run` and `test submit` (§11.3).
pub fn start(ctx: &Ctx, args: TestStartArgs, submit: bool) -> Res<()> {
    if let (Some(dir), Some(rid)) = (args.test_dir.clone(), args.results_id) {
        // Compute-node path (§11.6): no global DB, no registry, no MDB (D11).
        return run_compute(&dir, rid);
    }
    let machine = machine::resolve(ctx)?;
    let inst = Installation::resolve(ctx)?;
    let db = ctx.db.read()?;
    start_impl(
        &inst,
        &machine,
        &db,
        &args,
        submit,
        ctx.globals.verbose,
        ctx.globals.hostname.as_deref(),
    )
}

/// Assemble the test-run variable set: the §6.3 topology/walltime/machine
/// blocks plus the §11.9 test-only group. Deliberately does NOT include the
/// simulation-only names (SIMULATION_NAME, RUNDIR, PARFILE, …) — §11.9's
/// no-leak rule; a normal-partition script that needs them fails loudly at
/// substitution time rather than silently submitting a sim run.
#[allow(clippy::too_many_arguments)]
fn assemble_test_vars(
    name: &str,
    run_dir: &Path,
    results_id: u32,
    test_home: &Path,
    cfg: &ConfigMeta,
    cactus_root: &Path,
    machine: &Machine,
    topo: &Topology,
    select: &str,
    identity: &Identity,
    alias: &str,
    run_universe: Option<&str>,
) -> Res<VarSet> {
    let mut v = VarSet::new();
    set_topology_vars(&mut v, topo, name);
    // One job, one reservation — no splitting (§11.6).
    set_walltime_vars(&mut v, topo.total_wall, default_checkpt_buffer(topo.total_wall));
    let out_default = || run_dir.join("test.out").display().to_string();
    let err_default = || run_dir.join("test.err").display().to_string();
    v.set("STDOUT_FILE", topo.out.clone().unwrap_or_else(out_default));
    v.set("STDERR_FILE", topo.err.clone().unwrap_or_else(err_default));

    // Test-only group (§11.9).
    v.set("TEST_HOME", test_home.display().to_string());
    v.set("TEST_DIR", run_dir.display().to_string());
    v.set("TEST_NAME", name);
    v.set("RESULTS_ID", results_id as u64);
    v.set("TESTSUITE_RESULTS_DIR", results_dir(run_dir, results_id).display().to_string());
    // @TESTSUITE_SELECT@ carries the flesh's CCTK_TESTSUITE_RUN_TESTS format:
    // empty = run everything, else space-separated Thorn / Thorn/test entries
    // (metadata and logs keep the human-facing "all").
    v.set("TESTSUITE_SELECT", if select == "all" { "" } else { select });

    // Shared §6.3 names a test script uses (§11.9): the LIVE built binary —
    // a test run freezes no private copy (§11.5).
    v.set("SOURCEDIR", cactus_root.display().to_string());
    v.set("EXECUTABLE", build::executable_path(cactus_root, &cfg.name).display().to_string());
    v.set("CONFIGURATION", cfg.name.as_str());
    v.set("SCRIPTFILE", run_dir.join("submit-script").display().to_string());
    // Resolve @USER@/@ENV()@ (§4.2); the raw template would leak `@USER@`
    // literally (single-pass substitution). Matches the sim path.
    v.set("SCRATCH_HOME", machine.meta.resolved_paths()?.scratch_home.unwrap_or_default());
    v.set("ALIAS", alias);
    let cactup = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "cactup".to_owned());
    v.set("CACTUP", cactup);

    v.set("MACHINE", machine.name.as_str());
    v.set("HOSTNAME", identity.hostname.as_str());
    v.set("USER", identity.user.as_str());
    v.set("EMAIL", identity.email.as_str());
    v.set("EXECHOST", "");
    v.set("JOB_ID", "");

    set_machine_vars(&mut v, machine, &topo.queue, run_universe)?;
    v.set("RUNDEBUG", false);
    v.set("DEBUGGER", "gdb");
    Ok(v)
}

/// Many thorn tests read their data files as `../../../arrangements/…`, a
/// path that resolves because the flesh runs each test with cwd
/// `$TESTS_DIR/<config>/<thorn>` and TESTS_DIR defaults to `$CCTK_HOME/TEST`
/// — three levels below the source tree. With TESTS_DIR redirected to
/// `<run_dir>/results-NNNN` (§11.6), `../../..` lands in the run dir instead,
/// so plant an `arrangements` link there to keep those paths resolving.
fn link_arrangements(run_dir: &Path, cactus_root: &Path) -> Res<()> {
    let link = run_dir.join("arrangements");
    if fs::symlink_metadata(&link).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
        fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(cactus_root.join("arrangements"), &link)
        .with_context(|| format!("Failed to link {}", link.display()))
}

fn start_impl(
    inst: &Installation,
    machine: &Machine,
    db: &Database,
    args: &TestStartArgs,
    submit: bool,
    verbose: bool,
    hostname_override: Option<&str>,
) -> Res<()> {
    // §11.6: `test run` executes the suite HERE in the foreground, and the
    // flesh harness launches each test through the machine's parallel launcher
    // (e.g. `srun … $exe $parfile`), which needs a live job allocation. On a
    // login/submit node there is none, so every test silently produces no
    // output ("No files created in test directory"). Refuse up front when the
    // machine declares how to tell (allocation-env) and we're outside one.
    // `test submit` is exempt — it hands the suite to a compute node — and so
    // is the compute-node re-entry (handled earlier in `start`).
    if !submit && machine.meta.in_allocation() == Some(false) {
        bail!(
            "`cactup test run` executes the testsuite here in the foreground, and {}'s \
             test harness launches each test through a job launcher (e.g. srun) that needs \
             a live allocation — but none of [{}] is set, so this looks like a login/submit \
             node and every test would produce no output.\n\
             Run `cactup test submit` to execute the suite on a compute node, or grab an \
             interactive allocation first (e.g. `salloc` / `srun --pty`) and re-run \
             `cactup test run`.",
            machine.name,
            machine.meta.scheduler.allocation_env.join(", "),
        );
    }

    let inst_meta = inst.meta()?;
    let test_home = inst_meta.test_home()?.to_owned();
    let cactus_root = inst.cactus_root();

    // 1. Config: --config → the active config (fatal in the null-config
    //    state — §7.1).
    let cfg_name = match &args.config {
        Some(c) => c.clone(),
        None => inst_meta.active_config()?.to_owned(),
    };
    let cfg = ConfigMeta::load(&cactus_root, &cfg_name)?
        .ok_or_else(|| anyhow!("config \"{cfg_name}\" has never been built (`cactup config build {cfg_name}`)"))?;
    if !build::is_complete(&cactus_root, &cfg_name) {
        bail!("config \"{cfg_name}\" is incomplete; rebuild it (`cactup config build {cfg_name} -f`)");
    }
    // §11.5: a test run reads reference data straight from the live source
    // tree, so a tree that moved since the build matters here even more than
    // for a sim. Still only a notice — never a refusal, never a rebuild.
    crate::commands::delta::warn_if_sources_diverged(inst, &cfg, args.silent);

    // 2. Topology + queue/GPU guards (§4.4 / D12), exactly as a sim.
    let force_queue = args.force_queue || args.force;
    let mut topo = resolve_topology(&args.topology, machine, db, &cfg, force_queue)?;
    let select = if args.tests.is_empty() { "all".to_owned() } else { args.tests.join(" ") };

    // 3. TEST script variants for the queue (§11.2: prefer the test
    //    partition, fall back to normal) and the universes (§4.8); selection
    //    is driven by the config's BUILD universe (§4.4).
    let submit_scripts = machine.meta.script_variants(ScriptKind::Submit);
    let run_scripts = machine.meta.script_variants(ScriptKind::Run);
    let cfg_universe = cfg.universe.as_deref().unwrap_or(HOST_UNIVERSE);
    let (sub_variant, sub_entry) = submit_scripts.select(&topo.queue, cfg_universe, true, args.variant.as_deref())?;
    let (run_variant, run_entry) = run_scripts.select(&topo.queue, cfg_universe, true, args.variant.as_deref())?;

    // Testsuites run on a couple of ranks, not a full node: script-variant
    // `tasks` (§4.2, mode-relevant script first), else 2 (the ET convention;
    // §11.6). @TASKS@ feeds CCTK_TESTSUITE_RUN_PROCESSORS, i.e. $nprocs.
    let script_tasks = if submit {
        sub_entry.tasks.or(run_entry.tasks)
    } else {
        run_entry.tasks.or(sub_entry.tasks)
    };
    apply_tasks_default(&mut topo, &args.topology, script_tasks, Some(TESTSUITE_DEFAULT_TASKS));
    let submit_uni = resolve_submit_universe(
        machine,
        sub_entry.universe.as_deref(),
        submit_scripts.default_universe.as_deref(),
    )?;
    let submit_uni_name = submit_uni.as_ref().map(|(name, _)| name.as_str());
    let run_uni = resolve_run_universe(
        machine,
        &cfg,
        &args.universe,
        run_entry.universe.as_deref(),
        run_scripts.default_universe.as_deref(),
        verbose,
    )?;
    let run_uni_spec = run_uni.as_ref().map(|(name, u)| UniverseSpec::from_universe(name, u));
    let run_uni_name = run_uni.as_ref().map(|(name, _)| name.as_str());
    let (sub_variant, run_variant) = (sub_variant.to_owned(), run_variant.to_owned());

    // The run is named after its config (§11.3 has no name argument);
    // re-runs reuse the dir and allocate new result sets (§11.5).
    let name = cfg_name.clone();
    let run_dir = test_home.join(&cfg_name).join(&name);
    fs::create_dir_all(run_dir.join(".cactup"))
        .with_context(|| format!("Failed to create {}", run_dir.display()))?;
    link_arrangements(&run_dir, &cactus_root)?;

    // Register it (§11.8) under the per-installation lock.
    {
        let locked = inst.locked()?;
        let mut reg = locked.tests()?;
        if !reg.tests.contains_key(&name) {
            reg.tests.insert(
                name.clone(),
                TestEntry { dir: run_dir.clone(), config: cfg_name.clone(), created: Utc::now() },
            );
            locked.set_tests(&reg)?;
        }
    }

    let identity = Identity::resolve(db, hostname_override);
    let sched = Scheduler::new(&machine.meta);

    // A previous run of this config that is still in the queue must be dealt
    // with first (there is no reaper for tests — §11.6).
    if let Ok(old) = TestRun::open(&run_dir) {
        if old.meta.job_id != NO_JOB_ID
            && !LinkLock::is_held_live(&old.running_lock_path())?
            && matches!(
                sched.get_status(&old.meta.job_id)?,
                JobStatus::Running | JobStatus::Queued | JobStatus::Holding
            )
        {
            if !args.force {
                bail!(
                    "test run \"{name}\" still has a live job ({}); \
                     `cactup test stop {name}` first, or pass -f to stop it",
                    old.meta.job_id
                );
            }
            sched.stop(&old.meta.job_id)?;
        } else if LinkLock::is_held_live(&old.running_lock_path())? {
            bail!("test run \"{name}\" is currently executing (running.lock is held)");
        }
    }

    // 4. Result set: next results-%04d, or the active one with --overwrite/-f.
    let overwrite = args.overwrite || args.force;
    let mut run = TestRun {
        name: name.clone(),
        dir: run_dir.clone(),
        meta: TestMeta {
            schema: SCHEMA,
            name: name.clone(),
            config: cfg_name.clone(),
            config_id: cfg.config_id.clone(),
            build_id: cfg.build_id.clone(),
            machine: machine.name.clone(),
            alias: inst.alias.clone(),
            test_run_id: test_run_id(&name, &machine.name, &identity.hostname),
            select: select.clone(),
            queue: topo.queue.clone(),
            nodes: topo.nodes,
            tasks: topo.tasks,
            tpn: topo.tpn,
            cpus: topo.cpus,
            walltime: topo.total_wall,
            allocation: topo.allocation.clone(),
            job_id: NO_JOB_ID.to_owned(),
            status: None,
            universe: run_uni_spec,
            results: None,
            timestamps: Timestamps { created: Some(Utc::now()), ..Default::default() },
            vars: Default::default(),
        },
    };
    let lock = run.lock()?;
    let results_id = match (overwrite, active_results_id(&run_dir)?) {
        (true, Some(active)) => active,
        _ => next_results_id(&run_dir)?,
    };
    fs::create_dir_all(results_dir(&run_dir, results_id))
        .with_context(|| "Failed to create the results directory")?;
    activate_results(&run_dir, results_id)?;

    // Topology variable set (§8.5) + the §11.9 test-only group.
    let vset = assemble_test_vars(
        &name,
        &run_dir,
        results_id,
        &test_home,
        &cfg,
        &cactus_root,
        machine,
        &topo,
        &select,
        &identity,
        &inst.alias,
        run_uni_name,
    )?;
    run.meta.vars = freeze_vars(&vset);

    // 5. Scripts at the run root (§11.5): run-script always (the compute node
    //    executes it), submit-script for `test submit`.
    let run_script = generate_script(machine, ScriptKind::Run, &run_variant, &vset, Phase::Run, run_uni_name)?;
    write_executable(&run_dir.join("run-script"), &run_script)?;

    if submit {
        // Submit-phase ENV_SETUP under the submit universe (§6.1).
        let mut submit_vars = vset.clone();
        submit_vars.set("ENV_SETUP", machine.meta.effective_env(submit_uni_name, Phase::Submit));
        let submit_script =
            generate_script(machine, ScriptKind::Submit, &sub_variant, &submit_vars, Phase::Submit, submit_uni_name)?;
        write_executable(&run_dir.join("submit-script"), &submit_script)?;
        run.store_meta()?;

        let job_id = sched.submit(&submit_vars, submit_uni.as_ref().map(|(n, u)| (n.as_str(), *u)))?;
        run.meta.job_id = job_id.clone();
        run.meta.timestamps.submitted = Some(Utc::now());
        run.store_meta()?;
        run.log("test-submit", &format!("submitted {} as job {job_id} (select: {select})", results_name(results_id)));
        println!(
            "Submitted test run {} ({}) as job {}",
            name.bold(),
            results_name(results_id),
            job_id.bold()
        );
        Ok(())
    } else {
        run.meta.job_id = std::process::id().to_string();
        run.store_meta()?;
        drop(lock);
        run.log("test-run", &format!("running {} in the foreground (select: {select})", results_name(results_id)));
        println!("Running testsuite of {} into {}", name.bold(), results_name(results_id));
        execute_testsuite(&mut run, results_id, true)
    }
}

/// The compute-node path (§11.6): `--test-dir` + `--results-id` identify the
/// result set; everything else comes from `.cactup/test.toml`.
fn run_compute(dir: &Path, results_id: u32) -> Res<()> {
    let mut run = TestRun::open(dir)?;
    {
        let _lock = run.lock()?;
        // The set was activated at submit time; re-assert defensively.
        fs::create_dir_all(results_dir(dir, results_id))?;
        activate_results(dir, results_id)?;
    }
    run.log("test-run", &format!("compute-node run of {}", results_name(results_id)));
    execute_testsuite(&mut run, results_id, false)
}

/// Drive the testsuite (§11.6 steps 5–7): execute the stored run-script in
/// the frozen run universe, holding `running.lock` + heartbeat; then parse
/// the harness summary, record it, and exit non-zero on failures.
fn execute_testsuite(run: &mut TestRun, results_id: u32, tee: bool) -> Res<()> {
    let vset = thaw_vars(&run.meta.vars)?;
    // Re-assert the arrangements link like the results dir above: a run dir
    // from an older cactup may predate it.
    if let Some(src) = vset.get("SOURCEDIR") {
        link_arrangements(&run.dir, Path::new(&src.canonical()))?;
    }
    let script = run.dir.join("run-script");
    let universe: Option<Universe> = run.meta.universe.as_ref().map(|u| u.to_universe());
    let cmd = script_command(&script, universe.as_ref(), &vset, &run.dir)?;

    let running = LinkLock::acquire(&run.running_lock_path())?.with_heartbeat();
    let _ = fs::write(run.heartbeat_path(), b"");
    run.meta.status = Some("R".to_owned());
    run.store_meta()?;

    let tee_files = tee.then(|| (run.dir.join("test.out"), run.dir.join("test.err")));
    let status = spawn_and_wait(cmd, &run.heartbeat_path(), tee_files);
    drop(running);

    run.meta.status = Some("U".to_owned());
    run.meta.timestamps.finished = Some(Utc::now());

    // 7. Parse the harness's pass/fail summary (§11.6).
    let summary = read_summary(&results_dir(&run.dir, results_id), &run.meta.config);
    if let Some((passed, failed)) = summary {
        run.meta.results = Some(ResultsSummary { passed, failed, results_id });
    }
    run.store_meta()?;

    let status = status?;
    if !status.success() {
        run.log("test-run", &format!("testsuite run failed ({status})"));
        bail!("the testsuite run-script exited unsuccessfully ({status})");
    }
    match summary {
        Some((passed, failed)) => {
            run.log("test-run", &format!("{passed} passed, {failed} failed"));
            let line = format!("Testsuite {}: {passed} passed, {failed} failed", run.name.bold());
            if failed > 0 {
                println!("{}", line.bright_red());
                // CI-usable: non-zero when any test failed (§11.6).
                bail!("{failed} test(s) failed (results in {})", results_dir(&run.dir, results_id).display());
            }
            println!("{}", line.bright_green());
        }
        None => {
            run.log("test-run", "completed, but no summary.log was found");
            eprintln!(
                "{} the harness left no parsable summary in {}",
                "warning:".yellow().bold(),
                results_dir(&run.dir, results_id).display()
            );
        }
    }
    Ok(())
}

/// Parse the flesh testsuite `summary.log` for the passed/failed counts.
/// Tolerant of the harness's wording variants (`Number passed`, `Number of
/// tests passed`, `->`/`:`/`=` separators).
fn parse_summary(text: &str) -> Option<(u32, u32)> {
    let grab = |what: &str| -> Option<u32> {
        let re = Regex::new(&format!(r"(?im)^\s*number\s+(?:of\s+tests\s+)?{what}\s*(?:->|:|=)\s*(\d+)"))
            .ok()?;
        re.captures(text)?.get(1)?.as_str().parse().ok()
    };
    Some((grab("passed")?, grab("failed")?))
}

fn read_summary(results: &Path, config: &str) -> Option<(u32, u32)> {
    // The flesh writes $TESTS_DIR/<config>/summary.log (RunTestUtils.pl, with
    // TESTS_DIR pointed at the results dir); tolerate a harness that wrote at
    // the results root instead.
    let text = fs::read_to_string(results.join(config).join("summary.log"))
        .or_else(|_| fs::read_to_string(results.join("summary.log")))
        .ok()?;
    parse_summary(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{TopologyFlags, UniverseFlags};
    use crate::mdb::Layer;
    use crate::testsuite::list_results_ids;
    use crate::walltime::Walltime;

    /// A machine with BOTH normal and test-marked script variants (§11.2) and
    /// a fake echo scheduler.
    fn fake_machine(dir: &Path) -> Machine {
        fs::create_dir_all(dir.join("submitscripts")).unwrap();
        fs::create_dir_all(dir.join("runscripts")).unwrap();
        // Normal scripts exist but must never be picked while test ones exist.
        fs::write(dir.join("submitscripts/default.sh"), "#!/bin/sh\necho normal\n").unwrap();
        fs::write(dir.join("runscripts/default.sh"), "#!/bin/sh\necho normal\n").unwrap();
        fs::write(
            dir.join("submitscripts/test.sh"),
            "#!/bin/sh\nexec @CACTUP@ test run @TEST_NAME@ \
             --installation=@ALIAS@ --test-dir=@TEST_DIR@ --machine=@MACHINE@ \
             --results-id=@RESULTS_ID@\n",
        )
        .unwrap();
        // The test runscript fakes the flesh harness: like RunTestUtils.pl
        // with TESTS_DIR pointed at the results dir, it writes summary.log
        // under <results>/<config>/ (§11.6 step 6). The wording matches the
        // real "Number of tests passed -> N" / "Number failed -> N" lines.
        fs::write(
            dir.join("runscripts/test.sh"),
            "#!/bin/sh\ncd @SOURCEDIR@\n\
             mkdir -p @TESTSUITE_RESULTS_DIR@/@CONFIGURATION@\n\
             printf 'Number of tests passed -> 5\\nNumber failed -> %s\\n' \"$(cat @SOURCEDIR@/failcount)\" \
             > @TESTSUITE_RESULTS_DIR@/@CONFIGURATION@/summary.log\n\
             echo \"selection=@TESTSUITE_SELECT@ procs=@TASKS@ config=@CONFIGURATION@\" \
             > @TEST_DIR@/harness.txt\n",
        )
        .unwrap();
        let meta: crate::mdb::Meta = toml::from_str(
            r#"
            [machine]
            name = "fake"

            [hardware]
            max-cpus-per-node = 4

            [scheduler]
            submit = "echo [JOB-T@RESULTS_ID@]"
            submit-pattern = '\[(JOB-[^\]]*)\]'
            get-status = "true"
            status-pattern = "$^"
            queued-pattern = "$^"
            running-pattern = "$^"
            holding-pattern = "$^"
            stop = "true"

            [queues.local]
            default = true
            max-walltime = "8:00:00"

            [variants.submitscript]
            "default" = { queues = ["local"], default = true }
            "test" = { queues = ["local"], test = true, default = true }

            [variants.runscript]
            "default" = { queues = ["local"], default = true }
            "test" = { queues = ["local"], test = true, default = true }

            [variants.optionlist]
            variants = []
            "#,
        )
        .unwrap();
        Machine { name: "fake".to_owned(), dir: dir.to_owned(), layer: Layer::System, meta }
    }

    /// An installation with a COMPLETE (normal) active config named "tests".
    fn fake_installation(root: &Path) -> Installation {
        let inst = Installation::new("et", root);
        let test_home = root.join("testhome");
        fs::create_dir_all(&test_home).unwrap();
        {
            let locked = inst.locked().unwrap();
            let mut meta = locked.meta().unwrap();
            meta.active_config = Some("tests".to_owned());
            meta.test_home = Some(test_home);
            locked.set_meta(&meta).unwrap();
        }
        let root = inst.cactus_root();
        let cfg_dir = root.join("configs").join("tests");
        fs::create_dir_all(cfg_dir.join("config-data")).unwrap();
        fs::write(cfg_dir.join("config-data/cctk_Config.h"), "#define X\n").unwrap();
        fs::write(
            cfg_dir.join("cactup-config.toml"),
            r#"
            name = "tests"
            variant = "default"
            thornlist = "installation-default.th"
            machine = "fake"
            config-id = "config-tests-1"
            build-id = "build-tests-1"
            "#,
        )
        .unwrap();
        fs::create_dir_all(root.join("exe")).unwrap();
        fs::write(root.join("exe/cactus_tests"), "#!/bin/sh\n").unwrap();
        inst
    }

    fn test_args() -> TestStartArgs {
        TestStartArgs {
            silent: false,
            config: None,
            variant: None,
            force: false,
            overwrite: false,
            force_queue: false,
            universe: UniverseFlags { universe: None, no_universe: false },
            topology: TopologyFlags {
                allocation: None,
                queue: None,
                mail: None,
                mail_type: None,
                nodes: None,
                tasks: None,
                tpn: None,
                cpus: None,
                gpu: false,
                job_name: None,
                wall_time: Some(Walltime::parse("1:00:00").unwrap()),
                out: None,
                err: None,
            },
            tests: vec![],
            test_dir: None,
            results_id: None,
        }
    }

    #[test]
    fn submit_then_compute_run_records_results() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let db = Database::new();
        fs::write(inst.cactus_root().join("failcount"), "0").unwrap();

        // Submit: registers the run, activates results-0000, stores metadata.
        start_impl(&inst, &machine, &db, &test_args(), true, false, Some("testhost")).unwrap();

        let run_dir = tmp.path().join("inst/testhome/tests/tests");
        assert!(inst.tests().unwrap().tests.contains_key("tests"), "registered (§11.8)");
        // ../../../arrangements/… data paths in thorn tests must keep
        // resolving from $TESTS_DIR/<config>/<thorn> (link_arrangements).
        assert_eq!(
            fs::read_link(run_dir.join("arrangements")).unwrap(),
            inst.cactus_root().join("arrangements")
        );
        assert_eq!(active_results_id(&run_dir).unwrap(), Some(0));
        let run = TestRun::open(&run_dir).unwrap();
        assert_eq!(run.meta.job_id, "JOB-T0");
        assert_eq!(run.meta.select, "all");
        assert_eq!(run.meta.tasks, 2, "testsuite default, not fill-the-node (§11.6)");
        assert!(run.meta.timestamps.submitted.is_some());
        // The TEST-marked variants were selected, not the normal ones (§11.2).
        let submit_script = fs::read_to_string(run_dir.join("submit-script")).unwrap();
        assert!(submit_script.contains("--test-dir="), "{submit_script}");
        assert!(submit_script.contains("--results-id=0"), "{submit_script}");
        assert!(!submit_script.contains("normal"), "{submit_script}");

        // Compute-node path: executes the stored run-script, parses summary.
        run_compute(&run_dir, 0).unwrap();
        let run = TestRun::open(&run_dir).unwrap();
        let results = run.meta.results.as_ref().expect("summary recorded");
        assert_eq!((results.passed, results.failed, results.results_id), (5, 0, 0));
        assert!(run.meta.timestamps.finished.is_some());
        let harness = fs::read_to_string(run_dir.join("harness.txt")).unwrap();
        // Default selection substitutes as EMPTY — the flesh's "run all"
        // (CCTK_TESTSUITE_RUN_TESTS format); metadata keeps "all".
        assert_eq!(harness.trim(), "selection= procs=2 config=tests");

        // A second foreground run with failures: new result set, non-zero.
        fs::write(inst.cactus_root().join("failcount"), "2").unwrap();
        let mut args = test_args();
        args.tests = vec!["McLachlan/ML_BSSN".to_owned(), "Arrangement".to_owned()];
        let err = start_impl(&inst, &machine, &db, &args, false, false, Some("testhost"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 test(s) failed"), "{err}");
        assert_eq!(active_results_id(&run_dir).unwrap(), Some(1), "next set allocated + active");
        let run = TestRun::open(&run_dir).unwrap();
        let results = run.meta.results.as_ref().unwrap();
        assert_eq!((results.passed, results.failed, results.results_id), (5, 2, 1));
        assert_eq!(run.meta.select, "McLachlan/ML_BSSN Arrangement");

        // --overwrite reuses the active set instead of allocating (§11.5);
        // an explicit -T beats the testsuite tasks default.
        fs::write(inst.cactus_root().join("failcount"), "0").unwrap();
        let mut args = test_args();
        args.overwrite = true;
        args.topology.tasks = Some(4);
        start_impl(&inst, &machine, &db, &args, false, false, Some("testhost")).unwrap();
        assert_eq!(active_results_id(&run_dir).unwrap(), Some(1));
        assert_eq!(TestRun::open(&run_dir).unwrap().meta.tasks, 4);
        assert_eq!(list_results_ids(&run_dir).unwrap(), vec![0, 1]);
    }

    #[test]
    fn foreground_run_refuses_outside_allocation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut machine = fake_machine(&tmp.path().join("mdb-fake"));
        // Declare an allocation marker that is guaranteed unset in the test env,
        // so `in_allocation()` reports Some(false) (a login/submit node).
        machine.meta.scheduler.allocation_env = vec!["CACTUP_SURELY_UNSET_ALLOC".to_owned()];
        let inst = fake_installation(&tmp.path().join("inst"));
        let db = Database::new();
        fs::write(inst.cactus_root().join("failcount"), "0").unwrap();

        // Foreground `test run` is refused, with actionable guidance.
        let err = start_impl(&inst, &machine, &db, &test_args(), false, false, Some("h"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("allocation") && err.contains("test submit"), "{err}");

        // `test submit` hands the suite to a compute node, so it is exempt even
        // outside an allocation.
        start_impl(&inst, &machine, &db, &test_args(), true, false, Some("h")).unwrap();
    }

    #[test]
    fn null_config_fails_fast() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = Installation::new("et", tmp.path().join("bare"));
        {
            let locked = inst.locked().unwrap();
            let mut meta = locked.meta().unwrap();
            meta.test_home = Some(tmp.path().join("testhome"));
            locked.set_meta(&meta).unwrap();
        }
        let db = Database::new();
        let err = start_impl(&inst, &machine, &db, &test_args(), false, false, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("active config"), "guidance expected: {err}");
    }

    #[test]
    fn summary_parsing_variants() {
        let classic = "\
  Total available tests -> 451\n\
  Unrunnable tests      -> 12\n\
  Number passed         -> 431\n\
  Number failed         -> 8\n";
        assert_eq!(parse_summary(classic), Some((431, 8)));

        let wordy = "Number of tests passed: 10\nNumber of tests failed = 0\n";
        assert_eq!(parse_summary(wordy), Some((10, 0)));

        assert_eq!(parse_summary("no summary here"), None);
    }
}
