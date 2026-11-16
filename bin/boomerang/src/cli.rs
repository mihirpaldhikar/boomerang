use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "boomerang",
    version,
    about = "A bidirectional sync engine for data between SQLite and PostgreSQL databases"
)]
pub(crate) struct Cli {
    /// Path to the configuration file (required)
    #[arg(short, long, global = true, value_name = "FILE")]
    pub(crate) config: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Start the boomerang
    Start,
    /// Prune resources created by boomerang such as tables, triggers
    Prune,
}
