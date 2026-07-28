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

    /// Honor robots.txt (default: on). `--obey-robots=false` disables it —
    /// only for hosts you are authorized to crawl. Overrides
    /// OCCIPITAL_RESPECT_ROBOTS for this invocation.
    #[arg(long, global = true, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
    obey_robots: Option<bool>,
}

#[derive(Subcommand)]
enum Command {
    /// Search the web (cache-first).
    Search { query: String },
    /// Fetch a URL as reader-mode Markdown.
    Fetch {
        url: String,
        /// Bypass the cache and force a live fetch (parity with MCP/API).
        #[arg(long)]
        fresh: bool,
    },
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
    /// Neighbours of a curated page (shared entities / topic tags).
    Related {
        /// The distilled page to walk from.
        url: String,
        /// Max neighbours (default 5, max 20).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Show the recent request trail (what was sent, waited, and refused).
    Log {
        /// Rows to show (newest first).
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Run cache garbage-collection (decay-based pruning).
    Gc,
    /// Manage search-provider API keys (Brave / Tavily / Bing).
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// Inspect or clear the session cookie jar (needs OCCIPITAL_COOKIES=1).
    Cookies {
        #[command(subcommand)]
        action: CookiesAction,
    },
    /// Show config + tier + cache stats.
    Status,
}

/// Cookie values are credentials — never print them in full.
fn redact(value: &str) -> String {
    let n = value.chars().count();
    if n <= 4 {
        return "****".into();
    }
    let head: String = value.chars().take(2).collect();
    format!("{head}…({n} chars)")
}

/// `--field name=value` parser.
fn parse_field(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        .ok_or_else(|| format!("expected name=value, got {s:?}"))
}

#[derive(Subcommand)]
enum CookiesAction {
    /// List stored cookies (values redacted).
    List,
    /// Drop cookies for one domain, or all of them.
    Clear { domain: Option<String> },
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
    let mut config = Config::from_env()?;
    if let Some(obey) = cli.obey_robots {
        config.respect_robots = obey;
    }

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
        Command::Fetch { url, fresh } => {
            let engine = Engine::from_config(&config)?;
            let (page, from_cache) = engine.fetch(&url, fresh).await?;
            if from_cache {
                eprintln!("[served from cache]");
            }
            if page.salvaged {
                eprintln!("[salvaged from embedded page data]");
            }
            if page.js_required {
                eprintln!("[page requires JavaScript — nothing recoverable]");
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
        Command::Related { url, limit } => {
            let engine = Engine::from_config(&config)?;
            let report = engine.related(&url, limit).await?;
            for r in &report.related {
                let mut why = Vec::new();
                if !r.shared_entities.is_empty() {
                    why.push(format!("entities: {}", r.shared_entities.join(", ")));
                }
                if !r.shared_tags.is_empty() {
                    why.push(format!("tags: {}", r.shared_tags.join(", ")));
                }
                println!("{:.1}  {}\n   {}\n   {}\n", r.score, r.url, r.summary_head, why.join(" · "));
            }
            println!(
                "{} neighbour(s) of {} ({} pages distilled)",
                report.related.len(), report.url, report.distilled_total
            );
        }
        Command::Status => {
            println!("occipital {}", occipital::version());
            println!("tier:      {:?}", config.tier());
            println!("provider:  {}", config.search_provider);
            println!("db:        {}", config.db_path.display());
            println!("robots:    {}", config.respect_robots);
            println!("rate/dom:  {} req/s", config.rate_per_domain);
            println!(
                "cookies:   {}",
                if config.cookies_enabled {
                    format!("on ({})", config.cookies_file.display())
                } else {
                    "off".into()
                }
            );
            println!("proxy:     {}", config.proxy.as_deref().unwrap_or("(none)"));
            println!(
                "headers:   {}",
                config.headers_file.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(none)".into())
            );
            println!("curation:  {:?}", config.curate.backend);
            println!(
                "auto:      {:?} (every {}s, cap {}/24h)",
                config.curate.auto, config.curate.auto_interval_secs,
                if config.curate.auto_cap == 0 { "∞".to_string() } else { config.curate.auto_cap.to_string() }
            );
        }
        Command::Log { limit } => {
            let engine = Engine::from_config(&config)?;
            let rows = engine.log(limit)?;
            if rows.is_empty() {
                println!("(no requests logged)");
            }
            for r in &rows {
                let status = match (&r.status, &r.error) {
                    (Some(s), _) => s.to_string(),
                    (None, Some(e)) => format!("— {e}"),
                    (None, None) => "—".into(),
                };
                println!(
                    "{}  {:<4} {}  [{}]  waited {}ms, took {}ms",
                    r.at, r.method, r.url, status, r.wait_ms, r.duration_ms
                );
            }
        }
        Command::Gc => {
            let engine = Engine::from_config(&config)?;
            let pruned = engine.gc()?;
            println!("garbage-collected {pruned} stale page(s)");
        }
        Command::Cookies { action } => {
            if !config.cookies_enabled {
                eprintln!("[cookies are off — set OCCIPITAL_COOKIES=1 to enable the session jar]");
            }
            let jar = occipital::CookieJar::load(&config.cookies_file, true);
            match action {
                CookiesAction::List => {
                    let all = jar.list();
                    if all.is_empty() {
                        println!("(no stored cookies)");
                    }
                    for c in all {
                        let scope = if c.host_only { c.domain.clone() } else { format!(".{}", c.domain) };
                        let exp = c.expires_rfc3339().unwrap_or_else(|| "session".into());
                        let flags = if c.secure { " secure" } else { "" };
                        println!("{scope}{}  {} = {}  (expires {exp}{flags})", c.path, c.name, redact(&c.value));
                    }
                }
                CookiesAction::Clear { domain } => {
                    let n = jar.clear(domain.as_deref());
                    println!("cleared {n} cookie(s)");
                }
            }
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
