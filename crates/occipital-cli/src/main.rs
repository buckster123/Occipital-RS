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
    /// Show a page's element registry — links + forms with stable ordinals.
    Dom { url: String },
    /// Click an element by registry ordinal: link:N follows it, form:N submits it.
    Click { url: String, element: String },
    /// Fill and submit a form by registry ordinal.
    Submit {
        url: String,
        /// The form's 1-based ordinal (see `occipital dom`).
        #[arg(long)]
        form: usize,
        /// Field override as name=value (repeatable).
        #[arg(long = "field", value_parser = parse_field)]
        fields: Vec<(String, String)>,
    },
    /// Recall from already-read (cached) pages only.
    Recall { query: String },
    /// Distill cached pages into curated knowledge (summary/points/entities/tags).
    Distill {
        /// Distill this page (fetched first if not cached); omit to sweep.
        url: Option<String>,
        /// Sweep size when no URL is given (default 3, max 10).
        #[arg(long)]
        limit: Option<usize>,
    },
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

/// `--field name=value` parser.
fn parse_field(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        .ok_or_else(|| format!("expected name=value, got {s:?}"))
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::from_env()?;

    match cli.command {
        Command::Search { query } => {
            let engine = Engine::from_config(&config)?;
            let (results, from_cache) = engine.search(&query, None, false).await?;
            println!("# {} results via {}{}", results.len(), engine.provider_name(),
                if from_cache { " [cached]" } else { "" });
            for r in &results {
                println!("\n{}. {}\n   {}\n   {}", r.rank + 1, r.title, r.url, r.snippet);
            }
        }
        Command::Fetch { url } => {
            let engine = Engine::from_config(&config)?;
            let (page, from_cache) = engine.fetch(&url, false).await?;
            if from_cache {
                eprintln!("[served from cache]");
            }
            println!("{}", page.markdown);
        }
        Command::Dom { url } => {
            let engine = Engine::from_config(&config)?;
            let view = engine.dom(&url, false).await?;
            if view.from_cache {
                eprintln!("[served from cache]");
            }
            println!("{}  {}", view.url, view.title.as_deref().unwrap_or("(untitled)"));
            println!("snapshot: {}", if view.snapshot { "held" } else { "expired/none" });
            if !view.forms.is_empty() {
                println!("\nforms:");
                for f in &view.forms {
                    let fields = f.fields.iter()
                        .map(|x| format!("{} \"{}\"", x.kind, x.name))
                        .collect::<Vec<_>>().join(" · ");
                    println!("  #{} {} {} — {}", f.idx, f.method.to_uppercase(), f.action, fields);
                }
            }
            if !view.links.is_empty() {
                println!("\nlinks:");
                for l in &view.links {
                    println!("  #{} {} — {}", l.idx, l.text, l.url);
                }
            }
        }
        Command::Click { url, element } => {
            let engine = Engine::from_config(&config)?;
            let r = engine.click(&url, &element).await?;
            eprintln!("[{} → {}]", r.element, r.target_url);
            if let Some(s) = r.status {
                eprintln!("[status {s}]");
            }
            if r.from_cache {
                eprintln!("[served from cache]");
            }
            println!("{}", r.page.markdown);
        }
        Command::Submit { url, form, fields } => {
            let engine = Engine::from_config(&config)?;
            let r = engine.submit(&url, form, &fields).await?;
            let sent = r.sent.iter()
                .map(|f| format!("{}={}", f.name, f.value))
                .collect::<Vec<_>>().join(" ");
            eprintln!("[form#{} {} {} — {}]", r.form, r.method.to_uppercase(), r.action, sent);
            if let Some(s) = r.status {
                eprintln!("[status {s}]");
            }
            if r.cached {
                eprintln!("[served from cache]");
            }
            println!("{}", r.page.markdown);
        }
        Command::Recall { query } => {
            let engine = Engine::from_config(&config)?;
            let hits = engine.recall(&query, None).await?;
            if hits.is_empty() {
                println!("(nothing recalled)");
            }
            for h in &hits {
                let score = h.score.map(|s| format!("{s:.3}")).unwrap_or_else(|| "—".into());
                let tags = if h.tags.is_empty() { String::new() } else { format!("  [{}]", h.tags.join(", ")) };
                println!("{score}  {}{tags}\n   {}\n", h.url, h.snippet);
            }
        }
        Command::Distill { url, limit } => {
            let engine = Engine::from_config(&config)?;
            let report = engine.distill(url.as_deref(), limit).await?;
            for d in &report.distilled {
                let cached = if d.from_cache { " [already distilled]" } else { "" };
                println!("✓ {}{cached}\n   {}\n   tags: {}\n", d.url, d.summary, d.tags.join(", "));
            }
            for f in &report.failed {
                println!("✗ {}\n   {}\n", f.url, f.error);
            }
            println!(
                "distilled {} page(s), {} failed, {} still pending",
                report.distilled.len(), report.failed.len(), report.remaining
            );
        }
        Command::Status => {
            println!("occipital {}", occipital::version());
            println!("tier:      {:?}", config.tier());
            println!("provider:  {}", config.search_provider);
            println!("db:        {}", config.db_path.display());
            println!("robots:    {}", config.respect_robots);
            println!("rate/dom:  {} req/s", config.rate_per_domain);
            println!("curation:  {:?}", config.curate.backend);
            println!(
                "auto:      {:?} (every {}s, cap {}/24h)",
                config.curate.auto, config.curate.auto_interval_secs,
                if config.curate.auto_cap == 0 { "∞".to_string() } else { config.curate.auto_cap.to_string() }
            );
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
    }
    Ok(())
}
