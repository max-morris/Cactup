//! `cactup sim` — dispatch into the simulation subsystem (spec §8, §9).

use super::Ctx;
use crate::args::SimCommand;
use crate::installation::Installation;
use crate::sim;
use crate::Res;

pub fn dispatch(ctx: &Ctx, cmd: SimCommand) -> Res<()> {
    match cmd {
        SimCommand::Create { force, sim, parfile, config, sim_dir } => {
            let machine = super::machine::resolve(ctx)?;
            let inst = Installation::resolve(ctx)?;
            sim::create(
                ctx,
                &machine,
                &inst,
                force,
                &sim,
                &parfile,
                config.as_deref(),
                sim_dir.as_deref(),
            )?;
            Ok(())
        }
        SimCommand::Submit(args) => sim::start::submit(ctx, args),
        SimCommand::Run(args) => sim::start::run(ctx, args),
        SimCommand::Stop { sim, force } => sim::manage::stop(ctx, &sim, force),
        SimCommand::Clean { sim } => sim::manage::clean(ctx, &sim),
        SimCommand::Delete { sim, force } => sim::manage::delete(ctx, &sim, force),
        SimCommand::List { long, all } => sim::manage::list(ctx, long, all),
        SimCommand::Show { sim, long } => sim::manage::show(ctx, &sim, long),
        SimCommand::OutputDir { sim, restart_id } => sim::manage::output_dir(ctx, &sim, restart_id),
        SimCommand::Log { sim } => sim::manage::log_cmd(ctx, &sim),
    }
}
