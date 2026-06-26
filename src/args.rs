use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "cactup")]
#[command(version = "0.1.0")]
#[command(about = "The best way to install Cactus", long_about = None)]
pub(crate) struct Args {
    #[clap(short, long, global = true)]
    pub verbose: bool,
    #[clap(long, global = true, default_value = "https://bitbucket.org/einsteintoolkit/manifest.git")]
    pub manifest_url: String,
    #[clap(short, long, global = true, default_value_t = false)]
    pub force: bool,
    #[clap(subcommand)]
    pub command: Commands
}

#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
    /// List available Einstein Toolkit releases
    List {
        #[clap(short, long, help = "List all releases instead of only the few most recent.")]
        all: bool
    },
    /// Show all Einstein Toolkit installations on the machine
    Show,
    /// Set the active Einstein Toolkit installation
    Use {
        #[clap(help = "The alias of the installation to activate.")]
        alias: String,
    },
    /// Install an Einstein Toolkit release
    Install {
        #[clap(help = "The release to install. If unspecified, the most recent release will be installed.")]
        release: Option<String>,
        #[clap(short, long, help = "The unique name of the installation. If unspecified, the release name will be used.")]
        alias: Option<String>,
        #[clap(short, long, help = "Assume default answers to all unspecified flags instead of prompting.")]
        silent: bool,
        #[clap(long, help = "The prefix to install to.")]
        install_prefix: Option<String>,
        #[clap(long, help = "Skip creating a symlink.")]
        no_symlink: bool,
        #[clap(long, help = "The directory in which to create the symlink.")]
        symlink_prefix: Option<String>,
        #[clap(long, help = "The name of the symlink to create.")]
        symlink_name: Option<String>,
    }
}