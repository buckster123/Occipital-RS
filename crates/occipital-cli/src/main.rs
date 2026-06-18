//! occipital — the CLI.
//!
//! Phase 0: argument surface + config probe. The subcommands print an honest
//! not-implemented note until their roadmap phases land (a CLI that silently
//! does nothing is worse than one that says so).

use clap::{Parser, Subcommand};
use occipital::{Config, Engine, Keys};

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
    /// Manage search-provider API keys (Brave / Tavily / Bing).
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// Show config + tier + cache stats.
    Status,
}

#[derive(Subcommand)]
enum KeysAction {
    /// Store a provider's API key (written 0600).
    Set { provider: String, key: String },
    /// List providers with a stored key (redacted).
    List,
    /// Remove a provider's stored key.
    Rm { provider: String },
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
        Command::Gc => {
            let engine = Engine::from_config(&config)?;
            let pruned = engine.gc()?;
            println!("garbage-collected {pruned} stale page(s)");
        }
        Command::Keys { action } => {
            let mut keys = Keys::load(&config.keys_file);
            match action {
                KeysAction::Set { provider, key } => {
                    keys.set(&provider, &key);
                    keys.save()?;
                    println!("stored key for {provider}");
                }
                KeysAction::List => {
                    let listed = keys.list();
                    if listed.is_empty() {
                        println!("(no stored keys)");
                    }
                    for (p, redacted) in listed {
                        println!("{p}: {redacted}");
                    }
                }
                KeysAction::Rm { provider } => {
                    if keys.remove(&provider) {
                        keys.save()?;
                        println!("removed key for {provider}");
                    } else {
                        println!("no stored key for {provider}");
                    }
                }
            }
        }
        Command::Search { .. } | Command::Fetch { .. } | Command::Recall { .. } => {
            // The live search/fetch/recall verbs land with the CLI surface (Phase 8);
            // the MCP server is the primary interface today.
            eprintln!("not implemented in the CLI yet (Phase 8) — use occipital-mcp");
            std::process::exit(2);
        }
    }
    Ok(())
}
