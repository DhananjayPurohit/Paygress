// `paygress-cli ci` — everything a repo needs to get CI on rented compute.
//
// The pieces already existed: an ngit-ci coordinator watching a repo's Nostr
// events, and `paygress-cli adapter` buying a sandbox per job. What did not
// exist was knowing how to point them at each other. That knowledge was three
// flags long and lived only in a README, which is the difference between a
// thing that works and a thing anyone can use:
//
//   --runner socket-adapter                 send jobs to us, not to the host
//   --adapter-socket <path>                 where we are listening
//   --act-container-daemon-socket <uri>     give the job the sandbox's daemon
//
// The last one is the reason a repo does not need its own docker bootstrap.
// Without it act refuses to mount a daemon into the job container, and every
// workflow that runs docker has to install one itself -- which is where the
// 259-line scripts come from. With it, and a provider advertising `docker`,
// the job talks to the sandbox's own daemon and the workflow needs nothing.

mod deploy;
mod up;

use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct CiArgs {
    #[command(subcommand)]
    pub command: CiCommand,
}

#[derive(Subcommand)]
pub enum CiCommand {
    /// Run the adapter and coordinator that give a repo CI on rented sandboxes
    Up(up::UpArgs),

    /// Put the coordinator itself on rented compute, so no machine is yours
    Deploy(deploy::DeployArgs),
}

pub async fn execute(args: CiArgs, verbose: bool) -> Result<()> {
    match args.command {
        CiCommand::Up(a) => up::execute(a, verbose).await,
        CiCommand::Deploy(a) => deploy::execute(a, verbose).await,
    }
}
