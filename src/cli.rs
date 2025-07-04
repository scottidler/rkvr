use clap::{Parser, Subcommand};
use std::path::PathBuf;

// Function to get log file path for help text
fn get_log_file_path_for_help() -> String {
    dirs::data_local_dir()
        .map(|d| d.join("rkvr").join("logs").join("rkvr.log"))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.local/share/rkvr/logs/rkvr.log".to_string())
}

#[derive(Parser, Debug)]
#[command(
    name = "rkvr",
    about = "A safe file archival and removal tool",
    version = env!("GIT_DESCRIBE"),
    author = "Scott A. Idler <scott.a.idler@gmail.com>",
    arg_required_else_help = true,
    after_help = format!("Logs are written to: {}", get_log_file_path_for_help())
)]
pub struct Cli {
    #[arg(short, long, help = "Path to config file")]
    pub config: Option<PathBuf>,

    #[arg(name = "targets")]
    pub targets: Vec<String>,

    #[command(subcommand)]
    pub action: Option<Action>,
}

#[derive(Parser, Clone, Debug)]
pub struct Args {
    #[arg(name = "targets")]
    pub targets: Vec<String>,
}

#[derive(Subcommand, Clone, Debug)]
pub enum Action {
    #[command(about = "bkup files")]
    Bkup(Args),
    #[command(about = "rmrf files [default]")]
    Rmrf(Args),
    #[command(about = "recover rmrf|bkup files")]
    Rcvr(Args),
    #[command(about = "list bkup files")]
    LsBkup(Args),
    #[command(about = "list rmrf files")]
    LsRmrf(Args),
    #[command(about = "bkup files and rmrf the local files")]
    BkupRmrf(Args),
}

impl Default for Action {
    fn default() -> Self {
        Action::Rmrf(Args { targets: vec![] })
    }
} 