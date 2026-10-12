//! Machine-local Tailscale controls; independent of project selection.
use std::path::PathBuf;
use std::process::ExitCode;

use crate::core::{error::Result, net, project::ProjectStore};
use clap::Subcommand;

#[derive(Subcommand)]
pub(super) enum NetCommand {
    /// Connect to the tailnet; print the login URL and QR when needed.
    Up {
        /// One-time key file, removed after successful login.
        #[arg(long)]
        auth_key_file: Option<PathBuf>,
    },
    /// Disconnect without deleting the machine's login state.
    Down,
    /// Delete the Tailscale login state.
    Logout,
    /// Show account, tailnet IPs and peer direct/DERP paths as JSON.
    Status,
    /// Install checksum-verified static Tailscale binaries for this machine.
    Install,
}

pub(super) fn run(command: NetCommand) -> Result<ExitCode> {
    let store = ProjectStore::open()?;
    match command {
        NetCommand::Up { auth_key_file } => net::up(store.root(), auth_key_file.as_deref())?,
        NetCommand::Down => net::down(store.root())?,
        NetCommand::Logout => net::logout(store.root())?,
        NetCommand::Status => println!(
            "{}",
            serde_json::to_string_pretty(&net::status(store.root())?)
                .map_err(|e| crate::core::error::KanbanError::Invalid(e.to_string()))?
        ),
        NetCommand::Install => net::install(store.root())?,
    }
    Ok(ExitCode::SUCCESS)
}
