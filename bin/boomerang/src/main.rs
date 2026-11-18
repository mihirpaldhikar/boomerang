/*
 * Copyright (c) Mihir Paldhikar
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the “Software”), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */
use crate::cli::{Cli, Commands};
use crate::config::BoomerangConfig;
use clap::{CommandFactory, Parser};

mod cli;
mod config;

#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let config_path = cli.config.unwrap_or_else(|| {
        Cli::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "the following required arguments were not provided:\n  --config <FILE>",
            )
            .exit()
    });

    let config_file = match tokio::fs::read_to_string(config_path).await {
        Ok(f) => f,
        Err(e) => {
            match e.kind() {
                std::io::ErrorKind::NotFound => {
                    eprintln!("config file not found")
                }
                _ => {
                    eprintln!("failed to read config file");
                }
            }
            std::process::exit(1);
        }
    };

    let cfg = match BoomerangConfig::from_yaml(&config_file) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("config error: {}", e.to_string());
            std::process::exit(1);
        }
    };

    let pg = match database::PostgresDatabase::new(cfg.database.url).await {
        Ok(pg) => pg,
        Err(e) => {
            eprintln!("database error: {}", e.to_string());
            std::process::exit(1);
        }
    };

    match cli.command {
        Commands::Start => {
            println!("Hello from boomerang!")
        }
        Commands::Prune => {
            if let Err(e) = pg.remove_setup().await {
                eprintln!("pruning failed: {}", e.to_string());
            }
            println!("all the tables, triggers related to boomerang have been removed.")
        }
    }
}
