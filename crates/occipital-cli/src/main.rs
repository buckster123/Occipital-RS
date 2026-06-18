//! occipital — the CLI.
//!
//! Phase 0: argument surface + config probe. The subcommands print an honest
//! not-implemented note until their roadmap phases land (a CLI that silently
//! does nothing is worse than one that says so).

use clap::{Parser, Subcommand};
use occipital::Config;

#[derive(Parser)]
#[command(name = "occipital", version, about = "The agent's reading cortex — web search, fetch, recall, cache ops")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Search the web (cache-first).
    Search { query: String },
    /// Fetch a URL as reader-mode Markdown.
    Fetch { url: String },
    /// Recall from already-read (cached) pages only.
    Recall { query: String },
    /// Run cache garbage-collection (decay-based pruning).
    Gc,
    /// Show config + tier + cache stats.
    Status,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::from_env()?;

    match cli.command {
        Command::Status => {
            println!("occipital {}", occipital::version());
            println!("tier:      {:?}", config.tier());
            println!("provider:  {}", config.search_provider);
            println!("db:        {}", config.db_path.display());
            println!("robots:    {}", config.respect_robots);
            println!("rate/dom:  {} req/s", config.rate_per_domain);
        }
        Command::Search { .. } | Command::Fetch { .. } | Command::Recall { .. } | Command::Gc => {
            eprintln!("not implemented yet (Phase 0 scaffold) — see docs/build-roadmap.md");
            std::process::exit(2);
        }
    }
    Ok(())
}
