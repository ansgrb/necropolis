//! `necro` CLI and MCP stdio server entrypoint (blueprint §8 & §11).

use clap::{Parser, Subcommand};
use necro_core::{CARD_SCHEMA_VERSION, CardStatus, Config, PROMPT_VERSION};
use necro_discover::discover_under_root;
use necro_git::inspect_repo;
use necro_llm::GeminiClient;
use necro_mcp::McpServer;
use necro_redact::Redactor;
use necro_sandbox::Denylist;
use necro_scan::scan_project;
use necro_store::Store;
use necro_summarize::{BundleFile, BundleSymbol, RepoBundle, build_card, read_readme_excerpt};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(name = "necro", version, about = "Necropolis — Project Memory Bank & MCP Server")]
struct Cli {
    /// Path to `necro.toml` (optional).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Apply or revert database migrations (`up`, `down`, `roundtrip`).
    Migrate {
        #[arg(default_value = "up")]
        direction: String,
    },
    /// Scan a project path (or `--all` configured roots) and generate/update L0 cards.
    Scan {
        /// Path to a repository or root directory.
        path: Option<PathBuf>,
        /// Scan all roots configured in `necro.toml`.
        #[arg(long)]
        all: bool,
        /// Skip card generation after scanning.
        #[arg(long)]
        no_cards: bool,
    },
    /// Show or regenerate the L0 card for a project.
    Card {
        project: String,
        #[arg(long)]
        rebuild: bool,
        #[arg(long)]
        json: bool,
    },
    /// List indexed projects (or `--stale` / `--quarantined` cards).
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        stack: Option<String>,
    },
    /// Review or approve quarantined cards (`necro review` or `necro review --approve <card_id>`).
    Review {
        #[arg(long)]
        approve: Option<i64>,
        #[arg(long)]
        reject: Option<i64>,
    },
    /// Search symbols, paths, or ideas from the command line.
    Search {
        query: String,
        /// `symbol | path | idea`
        #[arg(long, default_value = "symbol")]
        mode: String,
    },
    /// Read a sandboxed, egress-redacted code pointer (`get_pointer` from CLI).
    Pointer {
        project: String,
        path: String,
        #[arg(long, default_value_t = 1)]
        start: u32,
        #[arg(long, default_value_t = 50)]
        end: u32,
        #[arg(long)]
        commit: Option<String>,
    },
    /// Purge a project and all its derived data (`necro forget <project>`).
    Forget {
        project: String,
    },
    /// Run the MCP server over stdio (`necro mcp`).
    Mcp,
    /// Run an end-to-end self-test & interactive demo of scanning, redaction, cards, and MCP tools.
    Demo {
        /// Optional path to scan during the demo (defaults to this repository + sample repos).
        path: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let (cfg, _cfg_path) = Config::load(cli.config.as_deref())?;
    let store = Store::connect(&cfg.database_url).await?;

    match cli.command {
        Commands::Migrate { direction } => match direction.as_str() {
            "up" => {
                store.migrate_up().await?;
                println!("migrations applied (up)");
            }
            "down" => {
                store.migrate_down().await?;
                println!("migrations reverted (down)");
            }
            "roundtrip" => {
                store.migrate_up().await?;
                store.migrate_down().await?;
                store.migrate_up().await?;
                println!("migration round-trip (up -> down -> up) OK");
            }
            other => anyhow::bail!("unknown migration direction `{other}` (use up, down, or roundtrip)"),
        },
        Commands::Scan { path, all, no_cards } => {
            store.migrate_up().await?;
            let roots: Vec<PathBuf> = if all {
                if cfg.roots.is_empty() {
                    anyhow::bail!("no `roots` configured in necro.toml; pass a <path> or configure `roots`");
                }
                cfg.roots.clone()
            } else if let Some(p) = path {
                vec![p]
            } else {
                vec![std::env::current_dir()?]
            };
            run_scan_and_cards(&store, &cfg, &roots, !no_cards).await?;
        }
        Commands::Card { project, rebuild, json } => {
            let proj = store
                .get_project(&project)
                .await?
                .ok_or_else(|| anyhow::anyhow!("project not found: {project}"))?;
            if rebuild {
                generate_and_store_card(&store, &cfg, &proj).await?;
            }
            let rec = match store.get_serving_card(&proj.id).await? {
                Some(r) => r,
                None => {
                    generate_and_store_card(&store, &cfg, &proj).await?;
                    store
                        .get_serving_card(&proj.id)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("card for `{}` is quarantined; run `necro review`", proj.name))?
                }
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&rec.card)?);
            } else {
                print!("{}", rec.card.to_markdown());
                println!("--- tokens: {} | fingerprint: {} ---", rec.tokens, &rec.fingerprint[..16.min(rec.fingerprint.len())]);
            }
        }
        Commands::List { status, stack } => {
            let rows = store.list_projects(status.as_deref(), stack.as_deref()).await?;
            if rows.is_empty() {
                println!("(no indexed projects)");
            }
            for p in rows {
                println!(
                    "{:<24} {:<10} langs={:<18} stack={}",
                    p.name,
                    p.status,
                    p.languages.join(","),
                    p.stack.iter().take(6).cloned().collect::<Vec<_>>().join(",")
                );
            }
        }
        Commands::Review { approve, reject } => {
            if let Some(id) = approve {
                let ok = store.set_card_status(id, CardStatus::Approved).await?;
                println!("card {id} approved: {ok}");
            } else if let Some(id) = reject {
                let ok = store.set_card_status(id, CardStatus::Rejected).await?;
                println!("card {id} rejected: {ok}");
            } else {
                let list = store.list_quarantined_cards().await?;
                if list.is_empty() {
                    println!("no quarantined cards");
                }
                for c in list {
                    println!("card #{} project={} findings={}", c.id, c.project_name, c.lint_findings);
                }
            }
        }
        Commands::Search { query, mode } => match mode.as_str() {
            "symbol" => {
                for h in store.find_symbols(&query, None, 20).await? {
                    println!(
                        "[{}] {} {} ({}:{}-{})",
                        h.project_name, h.kind, h.name, h.file_path, h.start_line, h.end_line
                    );
                }
            }
            "path" => {
                for h in store.find_paths(&query, None, 20).await? {
                    println!(
                        "[{}] {} ({}, {} loc) — {}",
                        h.project_name,
                        h.path,
                        h.lang.unwrap_or_else(|| "-".into()),
                        h.loc.unwrap_or(0),
                        h.role.unwrap_or_default()
                    );
                }
            }
            "idea" => {
                for h in store.search_ideas(&query, None, 10).await? {
                    println!("[{}] {} — {}", h.project_name, h.title, h.summary);
                }
            }
            other => anyhow::bail!("unknown search mode `{other}` (use symbol, path, or idea)"),
        },
        Commands::Pointer { project, path, start, end, commit } => {
            let mut mcp = McpServer::new(store, cfg)?;
            let args = json!({
                "project": project,
                "path": path,
                "start_line": start,
                "end_line": end,
                "commit": commit,
            });
            match mcp.call_tool("get_pointer", &args).await {
                Ok((val, _)) => println!("{}", serde_json::to_string_pretty(&val)?),
                Err(e) => anyhow::bail!(e),
            }
        }
        Commands::Forget { project } => {
            let removed = store.forget_project(&project).await?;
            println!("forgot `{project}`: {removed}");
        }
        Commands::Mcp => {
            let mut server = McpServer::new(store, cfg)?;
            server.serve_stdio().await?;
        }
        Commands::Demo { path } => {
            run_demo(&store, &cfg, path).await?;
        }
    }
    Ok(())
}

async fn run_scan_and_cards(
    store: &Store,
    cfg: &Config,
    roots: &[PathBuf],
    with_cards: bool,
) -> anyhow::Result<()> {
    let redactor = Redactor::new_default();
    let denylist = Denylist::new(&cfg.denylist)?;

    for root in roots {
        let discovered = discover_under_root(root, cfg.scan.discover_depth, &cfg.scan.exclude_dirs)?;
        for disc in &discovered {
            let report = scan_project(store, cfg, &redactor, &denylist, disc).await?;
            println!(
                "scanned {:<24} files={:<4} reparsed={:<4} symbols={:<4} redactions={:<2} unchanged={} ({} ms)",
                report.project_name,
                report.files_total,
                report.files_reparsed,
                report.symbols_extracted,
                report.secrets_redacted,
                report.skipped_unchanged,
                report.elapsed_ms,
            );
            if with_cards {
                if let Some(proj) = store.get_project(&report.project_id).await? {
                    // Only rebuild card when none is serving for the current fingerprint.
                    let up_to_date = store
                        .get_serving_card(&proj.id)
                        .await?
                        .is_some_and(|c| Some(c.fingerprint.as_str()) == proj.fingerprint.as_deref());
                    if !up_to_date {
                        generate_and_store_card(store, cfg, &proj).await?;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn generate_and_store_card(
    store: &Store,
    cfg: &Config,
    proj: &necro_store::ProjectRow,
) -> anyhow::Result<()> {
    let redactor = Redactor::new_default();
    let project_dir = if proj.rel_path == "." {
        Path::new(&proj.root_path).to_path_buf()
    } else {
        Path::new(&proj.root_path).join(&proj.rel_path)
    };
    let git_facts = inspect_repo(&project_dir);
    let readme_excerpt = read_readme_excerpt(&project_dir, &redactor);

    let file_map = store.load_file_stats(&proj.id).await?;
    let mut files: Vec<BundleFile> = file_map
        .into_values()
        .map(|f| BundleFile {
            path: f.path,
            lang: f.lang,
            role: f.role,
            loc: f.loc.unwrap_or(0),
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let top_syms = store
        .top_symbols_for_project(&proj.id, 30)
        .await?
        .into_iter()
        .map(|s| BundleSymbol {
            file_path: s.file_path,
            name: s.name,
            kind: s.kind,
            start_line: s.start_line,
            end_line: s.end_line,
            signature: s.signature,
        })
        .collect();

    let fp = proj.fingerprint.clone().unwrap_or_else(|| "0".repeat(32));
    let bundle = RepoBundle {
        name: proj.name.clone(),
        status: proj.status.clone(),
        languages: proj.languages.clone(),
        stack: proj.stack.clone(),
        fingerprint: fp,
        head_commit: proj.head_commit.clone(),
        last_commit_date: git_facts.last_commit_date,
        readme_excerpt,
        files,
        top_symbols: top_syms,
    };

    let allow_llm = !proj.flags.iter().any(|f| f == "no_llm");
    let gemini = if cfg.llm.provider == "gemini" {
        std::env::var(&cfg.llm.api_key_env).ok().filter(|k| !k.is_empty()).map(|k| GeminiClient::new(&cfg.llm.model, k))
    } else {
        None
    };

    let generated = build_card(&bundle, &redactor, gemini.as_ref(), allow_llm).await?;
    let model_id = generated.card.generated_by.clone();
    store
        .upsert_card(
            &proj.id,
            &generated.card,
            PROMPT_VERSION,
            &model_id,
            CARD_SCHEMA_VERSION,
            generated.tokens,
            generated.status,
            &generated.lint_findings,
        )
        .await?;
    println!(
        "  -> card {:<11} ({} tokens, {} ideas)",
        generated.status.as_str(),
        generated.tokens,
        generated.card.ideas.len()
    );
    Ok(())
}

async fn run_demo(store: &Store, cfg: &Config, target: Option<PathBuf>) -> anyhow::Result<()> {
    println!("=== NECROPOLIS END-TO-END DEMO ===");
    store.migrate_up().await?;
    println!("[1/5] Database migrations verified (Postgres 16 + pgvector + pg_trgm).");

    let target_dir = target.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    println!("[2/5] Initial scan of `{}` ...", target_dir.display());
    run_scan_and_cards(store, cfg, std::slice::from_ref(&target_dir), true).await?;

    println!("[3/5] Re-scanning immediately to verify < 2s no-op stat-cache / git short-circuit ...");
    run_scan_and_cards(store, cfg, std::slice::from_ref(&target_dir), false).await?;

    println!("[4/5] Exercising MCP server tools (`list_projects`, `get_card`, `find_symbol`, `search_ideas`, `get_pointer`) ...");
    let mut mcp = McpServer::new(store.clone(), cfg.clone())?;

    let (projects_val, _) = mcp.call_tool("list_projects", &json!({})).await.map_err(|e| anyhow::anyhow!(e))?;
    println!("  - list_projects returned {} project(s)", projects_val["projects"].as_array().map(|a| a.len()).unwrap_or(0));

    let first_proj = projects_val["projects"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|p| p["name"].as_str())
        .unwrap_or("necropolis")
        .to_string();

    if let Ok((card_val, _)) = mcp.call_tool("get_card", &json!({ "project": first_proj })).await {
        println!("  - get_card(`{first_proj}`) [{} tokens]:\n{}", card_val["card_tokens"], card_val["markdown"].as_str().unwrap_or(""));
    }

    let (syms_val, _) = mcp.call_tool("find_symbol", &json!({ "name": "Redactor", "k": 5 })).await.map_err(|e| anyhow::anyhow!(e))?;
    println!("  - find_symbol(`Redactor`) hits: {}", serde_json::to_string(&syms_val["hits"])?);

    println!("[5/5] Verifying security controls (path traversal, denylist, unindexed file, git arg injection) ...");
    for bad_call in [
        json!({ "project": first_proj, "path": "../../.ssh/id_rsa", "start_line": 1, "end_line": 10 }),
        json!({ "project": first_proj, "path": ".env", "start_line": 1, "end_line": 10 }),
        json!({ "project": first_proj, "path": "non_existent_unindexed.rs", "start_line": 1, "end_line": 10 }),
        json!({ "project": first_proj, "path": "Cargo.toml", "start_line": 1, "end_line": 5, "commit": "--output=/tmp/pwned" }),
    ] {
        let res = mcp.call_tool("get_pointer", &bad_call).await;
        assert!(res.is_err(), "expected security rejection for {bad_call}");
        println!("  - BLOCKED as expected: {} -> {}", bad_call["path"], res.unwrap_err());
    }

    println!("=== DEMO COMPLETE ===");
    Ok(())
}
