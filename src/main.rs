
use eyre::Result;
use clap::{Parser, Subcommand};


#[derive(Parser)]
#[command(name = "rmrf", about = "tool for staging rmrf-ing or bkup-ing files")]
#[command(version = "0.1.0")]
#[command(author = "Scott A. Idler <scott.a.idler@gmail.com>")]
#[command(after_help = "after_help")]
#[command(arg_required_else_help = true)]
struct Cli {

}

#[derive(Subcommand)]
enum Actions {

}

fn main() -> Result<()> {
    let cli = Cli::parse();
    Ok(())
}
