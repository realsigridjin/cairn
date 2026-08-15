use anyhow::{bail, Context, Result};
use bytes::Bytes;
use cairn_uqa::{
    config::{
        validate_env_name, write_default_config, CairnConfig, StoreOverrides, DEFAULT_CONFIG_PATH,
    },
    embedding::EmbeddingInputType,
    index::{
        builder::{
            build_shards, read_jsonl, read_jsonl_unvalidated, validate_chunks,
            write_revision_stats_json, BuildOptions,
        },
        reader::read_shard_meta,
    },
    manifest::{
        upload_content_addressed_file, upload_revision_stats, upload_tombstones,
        upload_uqa_bundle_file, Catalog, RevisionManifest,
    },
    model::{ChunkInput, GlobalScoringContext, RevisionStats, SearchRequest},
    object_store::PutCondition,
    runtime::RuntimeLimits,
    server::{serve, ServerOptions, ServerScope},
    CairnRuntime,
};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Parser)]
#[command(
    name = "cairn",
    version,
    about = "Revisioned object-store-native RAG runtime",
    propagate_version = true
)]
#[command(
    after_help = "Quick start:\n  cairn init --tenant acme --kb handbook\n  export OPENROUTER_API_KEY=sk-or-...\n  cairn snapshot chunks.jsonl --shards 4 --dev-calibration\n  cairn search 'object storage retrieval'"
)]
struct Cli {
    /// Config file. Defaults to .cairn/config.toml or $CAIRN_CONFIG.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Override config with a local object-store directory.
    #[arg(long, global = true, conflicts_with = "object_gateway")]
    local_store: Option<PathBuf>,
    /// Override config with an HTTP/R2 object gateway.
    #[arg(long, global = true, conflicts_with = "local_store")]
    object_gateway: Option<String>,
    /// Environment variable holding the gateway bearer token.
    #[arg(long, global = true)]
    bearer_env: Option<String>,
    /// Machine-readable output for commands that support it.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EmbeddingProviderArg {
    Openrouter,
    External,
}

impl EmbeddingProviderArg {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Openrouter => "openrouter",
            Self::External => "external",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompletionShell {
    Bash,
    Elvish,
    Fish,
    PowerShell,
    Zsh,
}

impl From<CompletionShell> for clap_complete::Shell {
    fn from(value: CompletionShell) -> Self {
        match value {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Elvish => Self::Elvish,
            CompletionShell::Fish => Self::Fish,
            CompletionShell::PowerShell => Self::PowerShell,
            CompletionShell::Zsh => Self::Zsh,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a safe local development config.
    Init(InitArgs),
    /// Validate configuration, object-store connectivity, and optionally OpenRouter.
    Doctor(DoctorArgs),
    /// Show the effective configuration after defaults and path resolution.
    Config,
    /// Embed a JSONL corpus with OpenRouter and write a reusable pre-embedded JSONL.
    Embed(EmbedArgs),
    /// Build and atomically publish a complete live corpus in one command.
    #[command(visible_alias = "ingest")]
    Snapshot(SnapshotArgs),
    /// Build immutable cold-search shard(s) without publishing.
    Build(BuildArgs),
    /// Build revision-global BM25/calibration statistics.
    Stats(StatsArgs),
    /// Compact a canonical corpus after applying tombstones.
    Compact(CompactArgs),
    /// Atomically publish a new revision. Parent/revision are auto-derived by default.
    Publish(PublishArgs),
    /// Show the current HEAD revision.
    Head(ScopeArgs),
    /// Atomically move HEAD to an already committed revision.
    #[command(visible_alias = "rollback")]
    Promote(PromoteArgs),
    /// Search directly against the configured object store.
    #[command(visible_alias = "query")]
    Search(SearchArgs),
    /// Run the HTTP search server.
    Serve(ServeArgs),
    /// Materialize a warm UQA database (requires --features uqa).
    UqaBuild(UqaBuildArgs),
    /// Validate and print a local shard's metadata.
    InspectShard { path: PathBuf },
    /// Generate shell completion script to stdout.
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Destination config path. Defaults to --config/$CAIRN_CONFIG/.cairn/config.toml.
    #[arg(long)]
    path: Option<PathBuf>,
    #[arg(long)]
    tenant: Option<String>,
    #[arg(long)]
    kb: Option<String>,
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args, Clone)]
struct ScopeArgs {
    #[arg(long, env = "CAIRN_TENANT")]
    tenant: Option<String>,
    #[arg(long, env = "CAIRN_KB")]
    kb: Option<String>,
}

#[derive(Debug, Args, Clone)]
struct DoctorArgs {
    #[command(flatten)]
    scope: ScopeArgs,
    /// Make one tiny live OpenRouter embedding request.
    #[arg(long)]
    check_embedding: bool,
    /// Verify object-store create/head/delete permissions with a temporary probe object.
    #[arg(long)]
    check_write: bool,
    /// Run both --check-embedding and --check-write.
    #[arg(long)]
    full: bool,
}

#[derive(Debug, Args)]
struct EmbedArgs {
    /// JSONL with id/text/metadata; vector may be omitted.
    input: PathBuf,
    #[arg(long, short = 'o')]
    out: PathBuf,
    /// OpenRouter model ID. Defaults to config embedding.model.
    #[arg(long)]
    model: Option<String>,
    /// Output dimensions. Defaults to config embedding.dimensions.
    #[arg(long)]
    dimensions: Option<u32>,
    /// Preserve vectors already present and only embed missing rows.
    #[arg(long)]
    missing_only: bool,
    /// Replace an existing output file after a successful embedding run.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct SnapshotArgs {
    #[command(flatten)]
    scope: ScopeArgs,
    /// Complete live-corpus JSONL. Snapshot is a full replacement, not a delta.
    input: PathBuf,
    /// OpenRouter model ID. Defaults to config embedding.model.
    #[arg(long)]
    embedding_model: Option<String>,
    /// Output embedding dimension. Defaults to config embedding.dimensions or an existing vector dimension.
    #[arg(long)]
    embedding_dimensions: Option<u32>,
    /// Re-embed every chunk even when the JSONL already contains vectors.
    #[arg(long)]
    reembed: bool,
    /// Never call the embedding API; useful for fully pre-embedded corpora.
    #[arg(long, conflicts_with = "reembed")]
    no_embed: bool,
    /// Declare the provenance of precomputed vectors. Defaults to openrouter when CAIRN generated any vector, otherwise external.
    #[arg(long, value_enum)]
    embedding_provider: Option<EmbeddingProviderArg>,
    #[arg(long, default_value_t = 1)]
    shards: usize,
    #[arg(long, default_value_t = 64)]
    ivf_lists: usize,
    #[arg(long, default_value_t = 8)]
    kmeans_iterations: usize,
    #[arg(long, default_value_t = 128)]
    payload_block_size: usize,
    #[arg(long, default_value_t = 256)]
    exact_block_size: usize,
    #[arg(long, default_value = "cairn_standard_cjk_v1")]
    analyzer: String,
    #[arg(long)]
    scoring: Option<PathBuf>,
    #[arg(long, conflicts_with = "scoring")]
    dev_calibration: bool,
    #[arg(long, default_value = "standard_cjk")]
    warm_analyzer: String,
    /// Parent directory for temporary shard artifacts.
    #[arg(long, default_value = ".cairn/tmp")]
    work_dir: PathBuf,
}

#[derive(Debug, Args)]
struct BuildArgs {
    /// Canonical chunk JSONL.
    input: PathBuf,
    /// Output directory. CAIRN builds into a staging directory and swaps it in on success.
    #[arg(long, short = 'o')]
    out: PathBuf,
    /// Replace an existing output directory after a successful staged build.
    #[arg(long)]
    force: bool,
    #[arg(long, default_value_t = 1)]
    shards: usize,
    #[arg(long, default_value_t = 64)]
    ivf_lists: usize,
    #[arg(long, default_value_t = 8)]
    kmeans_iterations: usize,
    #[arg(long, default_value_t = 128)]
    payload_block_size: usize,
    #[arg(long, default_value_t = 256)]
    exact_block_size: usize,
    #[arg(long, default_value = "cairn_standard_cjk_v1")]
    analyzer: String,
    /// Also create revision-global stats from this complete live corpus.
    #[arg(long)]
    stats: Option<PathBuf>,
    #[arg(long, requires = "stats")]
    scoring: Option<PathBuf>,
    /// Explicitly allow uncalibrated development defaults when --scoring is omitted.
    #[arg(long, requires = "stats", conflicts_with = "scoring")]
    dev_calibration: bool,
    #[arg(long, default_value = "standard_cjk")]
    warm_analyzer: String,
}

#[derive(Debug, Args)]
struct StatsArgs {
    input: PathBuf,
    #[arg(long, short = 'o')]
    out: PathBuf,
    #[arg(long)]
    scoring: Option<PathBuf>,
    /// Explicitly allow uncalibrated development defaults when --scoring is omitted.
    #[arg(long, conflicts_with = "scoring")]
    dev_calibration: bool,
    #[arg(long, default_value = "cairn_standard_cjk_v1")]
    analyzer: String,
    #[arg(long, default_value = "standard_cjk")]
    warm_analyzer: String,
}

#[derive(Debug, Args)]
struct CompactArgs {
    input: PathBuf,
    #[arg(long)]
    tombstones: Option<PathBuf>,
    #[arg(long, short = 'o')]
    out: PathBuf,
    #[arg(long)]
    force: bool,
    #[arg(long, default_value_t = 1)]
    shards: usize,
    #[arg(long)]
    stats: PathBuf,
    #[arg(long)]
    scoring: Option<PathBuf>,
    #[arg(long, conflicts_with = "scoring")]
    dev_calibration: bool,
    #[arg(long, default_value_t = 64)]
    ivf_lists: usize,
    #[arg(long, default_value = "cairn_standard_cjk_v1")]
    analyzer: String,
    #[arg(long, default_value = "standard_cjk")]
    warm_analyzer: String,
}

#[derive(Debug, Args)]
struct PublishArgs {
    #[command(flatten)]
    scope: ScopeArgs,
    /// Defaults to current HEAD + 1 (or 1 for a new KB).
    #[arg(long)]
    revision: Option<u64>,
    /// Override the auto-derived current HEAD parent.
    #[arg(long)]
    parent: Option<u64>,
    /// Embedding provider. Inherited from parent; root revisions default to OpenRouter.
    #[arg(long, value_enum)]
    embedding_provider: Option<EmbeddingProviderArg>,
    /// Embedding model identifier. Inherited from parent when omitted.
    #[arg(long)]
    embedding_model: Option<String>,
    /// Vector dimension. Inferred from local shards or inherited from parent.
    #[arg(long)]
    dimension: Option<u32>,
    #[arg(long = "stats", alias = "stats-json")]
    stats: PathBuf,
    /// New shard. Repeat for multiple shards.
    #[arg(long = "shard")]
    shards: Vec<PathBuf>,
    /// Add all *.cairn files from this directory, sorted by name.
    #[arg(long)]
    shard_dir: Option<PathBuf>,
    /// Replace inherited shards instead of appending delta shards.
    #[arg(long)]
    replace_shards: bool,
    #[arg(long)]
    tombstones: Option<PathBuf>,
    /// Only safe after full compaction physically removed deleted IDs.
    #[arg(long, requires = "replace_shards")]
    drop_inherited_tombstones: bool,
    #[arg(long)]
    uqa_db: Option<PathBuf>,
    /// Validate and print the publication plan without uploading or changing HEAD.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct PromoteArgs {
    #[command(flatten)]
    scope: ScopeArgs,
    revision: u64,
    /// Verify the target revision and show the planned HEAD move without mutating it.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct SearchArgs {
    #[command(flatten)]
    scope: ScopeArgs,
    /// Text query. Use --query-file - to read it from stdin.
    query: Option<String>,
    /// Read the text query from a UTF-8 file. Use '-' for stdin.
    #[arg(long, conflicts_with = "query")]
    query_file: Option<PathBuf>,
    /// JSON array, e.g. '[0.1,0.2]'.
    #[arg(long)]
    vector: Option<String>,
    /// File containing a JSON vector array.
    #[arg(long, conflicts_with = "vector")]
    vector_file: Option<PathBuf>,
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long, default_value_t = 200)]
    candidates: usize,
    #[arg(long)]
    revision: Option<u64>,
    /// Metadata filters as JSON object.
    #[arg(long)]
    filters: Option<String>,
    #[arg(long, default_value_t = 256 * 1024 * 1024)]
    max_remote_bytes: u64,
    #[arg(long, default_value_t = 4096)]
    max_range_reads: u64,
    #[arg(long)]
    cache: Option<PathBuf>,
    #[arg(long, default_value_t = 8)]
    nprobe: usize,
    /// Disable automatic OpenRouter query embedding and run lexical-only unless --vector is supplied.
    #[arg(long)]
    lexical_only: bool,
}

#[derive(Debug, Args)]
struct UqaBuildArgs {
    input: PathBuf,
    #[arg(long, short = 'o')]
    out: PathBuf,
    #[arg(long)]
    revision: u64,
    #[arg(long, default_value = "standard_cjk")]
    analyzer: String,
    #[arg(long, default_value_t = 16)]
    hnsw_m: usize,
    #[arg(long, default_value_t = 200)]
    ef_construction: usize,
    #[arg(long, default_value_t = 128)]
    ef_search: usize,
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    #[arg(long)]
    cache: Option<PathBuf>,
    #[arg(long)]
    cache_max_bytes: Option<u64>,
    #[arg(long, default_value_t = 8)]
    nprobe: usize,
    #[arg(long, default_value_t = 256 * 1024 * 1024)]
    max_remote_bytes_per_query: u64,
    #[arg(long, default_value_t = 4096)]
    max_range_reads_per_query: u64,
    #[arg(long, default_value_t = 20_000)]
    max_candidate_limit: usize,
    #[arg(long)]
    allow_historical_revisions: bool,
    /// Environment variable containing the bearer token required by KB endpoints.
    /// When unset, loopback development remains unauthenticated. Non-loopback
    /// binds require a token unless --allow-unauthenticated is explicit.
    #[arg(long, default_value = "CAIRN_SERVER_TOKEN")]
    auth_token_env: String,
    /// Explicitly permit an unauthenticated non-loopback bind. Use only behind a
    /// trusted private ingress that already enforces authentication.
    #[arg(long)]
    allow_unauthenticated: bool,
    /// Restrict this server instance to the configured default tenant/KB. This
    /// is recommended for a DeepSeek Harness sidecar.
    #[arg(long, conflicts_with_all = ["allowed_tenant", "allowed_kb"])]
    restrict_to_default_scope: bool,
    /// Restrict authenticated KB routes to this tenant. Must be paired with
    /// --allowed-kb.
    #[arg(long, requires = "allowed_kb")]
    allowed_tenant: Option<String>,
    /// Restrict authenticated KB routes to this knowledge base. Must be paired
    /// with --allowed-tenant.
    #[arg(long, requires = "allowed_tenant")]
    allowed_kb: Option<String>,
    /// Disable automatic OpenRouter query embedding for text-only requests.
    #[arg(long)]
    lexical_only: bool,
}

#[derive(Debug, Serialize)]
struct PublishPlan {
    tenant: String,
    knowledge_base: String,
    revision: u64,
    parent_revision: Option<u64>,
    embedding_provider: String,
    embedding_model: String,
    dimension: u32,
    local_shards: Vec<String>,
    inherited_shards: usize,
    replace_shards: bool,
    drop_inherited_tombstones: bool,
    dry_run: bool,
}

#[derive(Debug, Clone, Copy)]
enum ParentExpectation {
    Auto,
    Exact(Option<u64>),
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize tracing subscriber: {error}"))?;

    if let Command::Init(args) = &cli.command {
        let path = args
            .path
            .clone()
            .or_else(|| cli.config.clone())
            .or_else(|| std::env::var_os("CAIRN_CONFIG").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        let config = write_default_config(
            &path,
            args.tenant.as_deref(),
            args.kb.as_deref(),
            args.force,
        )?;
        return match cli.format {
            OutputFormat::Json => print_json(&serde_json::json!({"path": path, "config": config})),
            OutputFormat::Human => {
                let base = path
                    .parent()
                    .filter(|dir| !dir.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let local_store = config.store.local.as_deref().map_or_else(
                    || PathBuf::from(".cairn/store"),
                    |store_path| {
                        if store_path.is_relative() {
                            base.join(store_path)
                        } else {
                            store_path.to_path_buf()
                        }
                    },
                );
                println!("Created {}", path.display());
                println!("Local store: {}", local_store.display());
                println!(
                    "Embedding: OpenRouter / {} / {} dimensions",
                    config.embedding.model, config.embedding.dimensions
                );
                println!("Next: export {}=sk-or-...", config.embedding.api_key_env);
                println!("      cairn snapshot <chunks.jsonl> --dev-calibration");
                Ok(())
            }
        };
    }

    let (config, loaded_path) = CairnConfig::load(cli.config.as_deref())?;
    let overrides = StoreOverrides {
        local: cli.local_store.clone(),
        object_gateway: cli.object_gateway.clone(),
        bearer_env: cli.bearer_env.clone(),
    };

    match cli.command {
        Command::Init(_) => unreachable_command(),
        Command::Doctor(args) => {
            doctor(
                &config,
                &overrides,
                loaded_path.as_deref(),
                args,
                cli.format,
            )
            .await
        }
        Command::Config => show_config(&config, loaded_path.as_deref(), cli.format),
        Command::Embed(args) => embed_command(&config, args, cli.format).await,
        Command::Snapshot(args) => snapshot(&config, &overrides, args, cli.format).await,
        Command::Build(args) => build(args, cli.format),
        Command::Stats(args) => stats(args, cli.format),
        Command::Compact(args) => compact(args, cli.format),
        Command::Publish(args) => publish(&config, &overrides, args, cli.format).await,
        Command::Head(scope) => head(&config, &overrides, scope, cli.format).await,
        Command::Promote(args) => promote(&config, &overrides, args, cli.format).await,
        Command::Search(args) => search(&config, &overrides, args, cli.format).await,
        Command::Serve(args) => serve_command(&config, &overrides, args).await,
        Command::UqaBuild(args) => uqa_build(args, cli.format),
        Command::InspectShard { path } => inspect_shard(&path, cli.format),
        Command::Completions { shell } => completions(shell),
    }
}

fn unreachable_command() -> Result<()> {
    bail!("internal command dispatch error")
}

fn show_config(
    config: &CairnConfig,
    loaded_path: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    config.validate()?;
    match format {
        OutputFormat::Json => print_json(&serde_json::json!({
            "config_path": loaded_path.map(|path| path.display().to_string()),
            "config": config,
            "embedding_api_key_set": std::env::var(&config.embedding.api_key_env)
                .ok()
                .is_some_and(|value| !value.trim().is_empty()),
        })),
        OutputFormat::Human => {
            println!(
                "Config: {}",
                loaded_path.map_or_else(
                    || "(built-in defaults)".to_owned(),
                    |path| path.display().to_string()
                )
            );
            println!(
                "Embedding: {}/{} ({}d)",
                config.embedding.provider, config.embedding.model, config.embedding.dimensions
            );
            println!("Embedding endpoint: {}", config.embedding.base_url);
            println!(
                "Embedding key env: {} ({})",
                config.embedding.api_key_env,
                if std::env::var(&config.embedding.api_key_env)
                    .ok()
                    .is_some_and(|value| !value.trim().is_empty())
                {
                    "set"
                } else {
                    "not set"
                }
            );
            if let Some(tenant) = config.defaults.tenant.as_deref() {
                println!("Default tenant: {tenant}");
            }
            if let Some(kb) = config.defaults.knowledge_base.as_deref() {
                println!("Default KB: {kb}");
            }
            if let Some(cache) = config.defaults.cache.as_deref() {
                println!("Cache: {}", cache.display());
            }
            match (&config.store.local, &config.store.object_gateway) {
                (Some(path), None) => println!("Store: local {}", path.display()),
                (None, Some(url)) => println!("Store: gateway {url}"),
                (None, None) => println!("Store: local default (.cairn/store)"),
                (Some(_), Some(_)) => bail!("config selects both local and object gateway"),
            }
            Ok(())
        }
    }
}

async fn doctor(
    config: &CairnConfig,
    overrides: &StoreOverrides,
    loaded_path: Option<&Path>,
    args: DoctorArgs,
    format: OutputFormat,
) -> Result<()> {
    config.validate()?;
    let store = config.resolve_store(overrides)?;
    let probe = store.head("_cairn_doctor_/missing-object").await?;
    let check_write = args.check_write || args.full;
    let check_embedding = args.check_embedding || args.full;
    let write_check = if check_write {
        let key = format!("_cairn_doctor_/probes/{}", uuid::Uuid::new_v4());
        let payload = Bytes::from_static(b"cairn-doctor");
        let created = store
            .put(&key, payload.clone(), PutCondition::CreateOnly)
            .await?
            .context("object-store write probe was not created")?;
        if created.size != payload.len() as u64 {
            let _ = store.delete(&key).await;
            bail!("object-store write probe size mismatch")
        }
        let observed = store
            .head(&key)
            .await?
            .context("object-store write probe was not readable")?;
        if observed.size != created.size {
            let _ = store.delete(&key).await;
            bail!("object-store write probe HEAD size mismatch")
        }
        store
            .delete(&key)
            .await
            .context("delete object-store write probe")?;
        Some(true)
    } else {
        None
    };
    let catalog = Catalog::new(store.clone());
    let scoped = resolve_scope_optional(config, args.scope)?;
    let head = if let Some((tenant, kb)) = scoped.as_ref() {
        catalog.get_head(tenant, kb).await?.map(|(head, _)| head)
    } else {
        None
    };
    let embedding_key_set = std::env::var(&config.embedding.api_key_env)
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let embedding_live = if check_embedding {
        let embedder = config.embedding.client()?;
        let vectors = embedder
            .embed(
                &config.embedding.model,
                config.embedding.dimensions,
                EmbeddingInputType::Query,
                &["cairn doctor"],
            )
            .await
            .context("OpenRouter embedding health check failed")?;
        Some(vectors.first().map_or(0, Vec::len))
    } else {
        None
    };
    let value = serde_json::json!({
        "ok": true,
        "config": loaded_path.map(|p| p.display().to_string()),
        "store_probe_found": probe.is_some(),
        "store_write_check": write_check,
        "embedding": {
            "provider": &config.embedding.provider,
            "model": &config.embedding.model,
            "dimensions": config.embedding.dimensions,
            "api_key_env": &config.embedding.api_key_env,
            "api_key_set": embedding_key_set,
            "live_check_dimensions": embedding_live,
        },
        "scope": scoped.as_ref().map(|(t, k)| serde_json::json!({"tenant": t, "kb": k})),
        "head": head,
    });
    match format {
        OutputFormat::Json => print_json(&value),
        OutputFormat::Human => {
            println!(
                "✓ config {}",
                loaded_path.map_or_else(|| "(defaults)".to_owned(), |p| p.display().to_string())
            );
            println!("✓ object store reachable");
            if write_check.is_some() {
                println!("✓ object store create/head/delete probe");
            } else {
                println!("  run `cairn doctor --check-write` to verify write permissions");
            }
            if let Some(dimension) = embedding_live {
                println!(
                    "✓ OpenRouter live check: {} ({}d)",
                    config.embedding.model, dimension
                );
            } else if embedding_key_set {
                println!(
                    "✓ OpenRouter credentials configured: {} ({}d)",
                    config.embedding.model, config.embedding.dimensions
                );
                println!("  run `cairn doctor --check-embedding` to test the API end-to-end");
            } else {
                println!(
                    "! {} is not set; automatic embedding is unavailable",
                    config.embedding.api_key_env
                );
            }
            if let Some((tenant, kb)) = scoped {
                match value.get("head").filter(|v| !v.is_null()) {
                    Some(head) => println!(
                        "✓ {tenant}/{kb} HEAD {}",
                        head.get("revision")
                            .and_then(|v| v.as_u64())
                            .unwrap_or_default()
                    ),
                    None => println!("• {tenant}/{kb} has no HEAD yet"),
                }
            }
            Ok(())
        }
    }
}

async fn embed_command(config: &CairnConfig, args: EmbedArgs, format: OutputFormat) -> Result<()> {
    if args.out.exists() && !args.force {
        bail!(
            "output already exists: {}; pass --force to replace it",
            args.out.display()
        )
    }
    let (chunks, provider, model) = prepare_snapshot_chunks(
        config,
        &args.input,
        args.model.as_deref(),
        args.dimensions,
        Some(EmbeddingProviderArg::Openrouter),
        !args.missing_only,
        false,
    )
    .await?;
    let dimension = chunks
        .first()
        .context("embedded corpus is empty")?
        .vector
        .len();
    write_chunks_jsonl_atomic(&args.out, &chunks, args.force)?;
    match format {
        OutputFormat::Json => print_json(&serde_json::json!({
            "output": args.out,
            "chunks": chunks.len(),
            "embedding_provider": provider,
            "embedding_model": model,
            "dimension": dimension,
        })),
        OutputFormat::Human => {
            println!(
                "✓ embedded {} chunks → {}",
                chunks.len(),
                args.out.display()
            );
            println!("  {provider}/{model} dim={dimension}");
            Ok(())
        }
    }
}

fn write_chunks_jsonl_atomic(path: &Path, chunks: &[ChunkInput], replace: bool) -> Result<()> {
    use std::io::Write as _;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    for chunk in chunks {
        serde_json::to_writer(&mut temporary, chunk)?;
        temporary.write_all(b"\n")?;
    }
    temporary.as_file().sync_all()?;
    if replace {
        temporary.persist(path).map_err(|error| error.error)?;
    } else {
        temporary
            .persist_noclobber(path)
            .map_err(|error| error.error)?;
    }
    cairn_uqa::config::sync_directory(parent)?;
    Ok(())
}

async fn snapshot(
    config: &CairnConfig,
    overrides: &StoreOverrides,
    args: SnapshotArgs,
    format: OutputFormat,
) -> Result<()> {
    if args.shards == 0 {
        bail!("--shards must be positive")
    }
    // Capture the lineage before expensive local indexing. If another writer
    // advances HEAD while the snapshot is being built, publication must fail
    // instead of silently replacing data that was committed concurrently.
    let (tenant, kb) = resolve_scope(config, args.scope.clone())?;
    let catalog = Catalog::new(config.resolve_store(overrides)?);
    let expected_parent = catalog
        .get_head(&tenant, &kb)
        .await?
        .map(|(head, _)| head.revision);
    let expected_revision = catalog
        .next_available_revision(&tenant, &kb, expected_parent)
        .await?;
    std::fs::create_dir_all(&args.work_dir)
        .with_context(|| format!("create work directory {}", args.work_dir.display()))?;
    let workspace = tempfile::Builder::new()
        .prefix("cairn-snapshot-")
        .tempdir_in(&args.work_dir)?;
    let shard_dir = workspace.path().join("shards");
    let stats_path = workspace.path().join("revision-stats.json");
    let (chunks, embedding_provider, embedding_model) = prepare_snapshot_chunks(
        config,
        &args.input,
        args.embedding_model.as_deref(),
        args.embedding_dimensions,
        args.embedding_provider,
        args.reembed,
        args.no_embed,
    )
    .await?;
    let scoring = read_scoring(args.scoring.as_deref(), args.dev_calibration)?;
    let options = BuildOptions {
        ivf_lists: args.ivf_lists,
        kmeans_iterations: args.kmeans_iterations,
        payload_block_size: args.payload_block_size,
        exact_block_size: args.exact_block_size,
        compression_level: 3,
        analyzer: args.analyzer.clone(),
    };
    let reports = build_shards(&chunks, &shard_dir, args.shards, &options)?;
    write_revision_stats_json(
        &chunks,
        scoring,
        args.analyzer,
        args.warm_analyzer,
        &stats_path,
    )?;
    if matches!(format, OutputFormat::Human) {
        let bytes = reports
            .iter()
            .map(|(_, report)| report.output_bytes)
            .sum::<u64>();
        eprintln!(
            "✓ built snapshot staging: {} chunks, {} shards, {} bytes",
            chunks.len(),
            reports.len(),
            bytes
        );
    }
    publish_with_expectation(
        config,
        overrides,
        PublishArgs {
            scope: ScopeArgs {
                tenant: Some(tenant),
                kb: Some(kb),
            },
            revision: Some(expected_revision),
            parent: expected_parent,
            embedding_provider: Some(match embedding_provider.as_str() {
                "openrouter" => EmbeddingProviderArg::Openrouter,
                _ => EmbeddingProviderArg::External,
            }),
            embedding_model: Some(embedding_model),
            dimension: None,
            stats: stats_path,
            shards: Vec::new(),
            shard_dir: Some(shard_dir),
            replace_shards: true,
            tombstones: None,
            drop_inherited_tombstones: true,
            uqa_db: None,
            dry_run: false,
        },
        format,
        ParentExpectation::Exact(expected_parent),
    )
    .await
}

fn build(args: BuildArgs, format: OutputFormat) -> Result<()> {
    let options = BuildOptions {
        ivf_lists: args.ivf_lists,
        kmeans_iterations: args.kmeans_iterations,
        payload_block_size: args.payload_block_size,
        exact_block_size: args.exact_block_size,
        compression_level: 3,
        analyzer: args.analyzer.clone(),
    };
    // Read the canonical corpus exactly once so shards and revision statistics
    // cannot observe different file contents if the input changes mid-command.
    let chunks = read_jsonl(&args.input)?;
    // Validate calibration intent before the expensive index build.
    let scoring = args
        .stats
        .as_ref()
        .map(|_| read_scoring(args.scoring.as_deref(), args.dev_calibration))
        .transpose()?;
    let reports = staged_shard_build(&chunks, &args.out, args.shards, &options, args.force)?;
    let stats_value = match (args.stats.as_ref(), scoring) {
        (Some(path), Some(scoring)) => Some(write_revision_stats_json(
            &chunks,
            scoring,
            args.analyzer,
            args.warm_analyzer,
            path,
        )?),
        (None, None) => None,
        _ => bail!("internal stats/scoring state mismatch"),
    };

    if matches!(format, OutputFormat::Json) {
        return print_json(&serde_json::json!({
            "shards": reports.iter().map(|(path, report)| serde_json::json!({"path": path, "report": report})).collect::<Vec<_>>(),
            "stats": stats_value,
        }));
    }
    for (path, report) in &reports {
        println!(
            "✓ {}  docs={} dim={} bytes={}",
            path.display(),
            report.documents,
            report.dimension,
            report.output_bytes
        );
    }
    if let Some(path) = args.stats {
        println!("✓ {}  revision stats", path.display());
    }
    Ok(())
}

fn staged_shard_build(
    chunks: &[ChunkInput],
    output: &Path,
    shards: usize,
    options: &BuildOptions,
    force: bool,
) -> Result<Vec<(PathBuf, cairn_uqa::index::builder::BuildReport)>> {
    if shards == 0 {
        bail!("--shards must be positive")
    }
    if output.exists() && !output.is_dir() {
        bail!(
            "output path exists and is not a directory: {}",
            output.display()
        )
    }
    if output.exists() && !force {
        bail!(
            "output directory already exists: {}; pass --force to replace it",
            output.display()
        )
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".cairn-build-")
        .tempdir_in(parent)?;
    let built = build_shards(chunks, staging.path(), shards, options)?;
    let final_reports = built
        .iter()
        .map(|(path, report)| {
            let name = path.file_name().context("staged shard has no filename")?;
            Ok((output.join(name), report.clone()))
        })
        .collect::<Result<Vec<_>>>()?;
    let staged_path = staging.keep();
    replace_directory(&staged_path, output)?;
    Ok(final_reports)
}

fn replace_directory(staged_path: &Path, output: &Path) -> Result<()> {
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if output.exists() {
        let backup = parent.join(format!(".cairn-old-{}", uuid::Uuid::new_v4()));
        std::fs::rename(output, &backup)
            .with_context(|| format!("move existing output {}", output.display()))?;
        match std::fs::rename(staged_path, output) {
            Ok(()) => {
                cairn_uqa::config::sync_directory(parent)?;
                if let Err(error) = std::fs::remove_dir_all(&backup) {
                    eprintln!("warning: replacement succeeded but old output cleanup failed at {}: {error}", backup.display());
                } else {
                    cairn_uqa::config::sync_directory(parent)?;
                }
            }
            Err(install_error) => match std::fs::rename(&backup, output) {
                Ok(()) => {
                    cairn_uqa::config::sync_directory(parent)?;
                    let _ = std::fs::remove_dir_all(staged_path);
                    return Err(install_error)
                        .with_context(|| format!("install staged output {}", output.display()));
                }
                Err(restore_error) => {
                    bail!(
                            "failed to install staged output {} ({install_error}) and failed to restore previous output from {} ({restore_error}); staged data remains at {}",
                            output.display(), backup.display(), staged_path.display()
                        )
                }
            },
        }
    } else if let Err(error) = std::fs::rename(staged_path, output) {
        let _ = std::fs::remove_dir_all(staged_path);
        return Err(error).with_context(|| format!("install staged output {}", output.display()));
    } else {
        cairn_uqa::config::sync_directory(parent)?;
    }
    Ok(())
}

fn stats(args: StatsArgs, format: OutputFormat) -> Result<()> {
    let chunks = read_jsonl(&args.input)?;
    let stats = write_revision_stats_json(
        &chunks,
        read_scoring(args.scoring.as_deref(), args.dev_calibration)?,
        args.analyzer,
        args.warm_analyzer,
        &args.out,
    )?;
    match format {
        OutputFormat::Json => print_json(&stats),
        OutputFormat::Human => {
            println!("✓ {}", args.out.display());
            println!(
                "  live_documents={} avg_doc_len={:.2} terms={}",
                stats.live_document_count,
                stats.average_document_length,
                stats.term_df.len()
            );
            Ok(())
        }
    }
}

fn compact(args: CompactArgs, format: OutputFormat) -> Result<()> {
    let chunks = read_jsonl(&args.input)?;
    let tombstones = match args.tombstones.as_ref() {
        Some(path) => cairn_uqa::compaction::read_tombstone_lines(path)?,
        None => BTreeSet::new(),
    };
    let options = BuildOptions {
        ivf_lists: args.ivf_lists,
        analyzer: args.analyzer.clone(),
        ..BuildOptions::default()
    };
    if args.out.exists() && !args.out.is_dir() {
        bail!(
            "output path exists and is not a directory: {}",
            args.out.display()
        )
    }
    if args.out.exists() && !args.force {
        bail!(
            "output directory already exists: {}; pass --force to replace it",
            args.out.display()
        )
    }
    let parent = args
        .out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".cairn-compact-")
        .tempdir_in(parent)?;
    let report = cairn_uqa::compaction::compact_canonical_chunks(
        &chunks,
        &tombstones,
        staging.path(),
        args.shards,
        &options,
        read_scoring(args.scoring.as_deref(), args.dev_calibration)?,
        &args.warm_analyzer,
    )?;
    let final_shards = report
        .shards
        .iter()
        .map(|(path, shard_report)| {
            let name = path
                .file_name()
                .context("staged compacted shard has no filename")?;
            Ok((args.out.join(name), shard_report.clone()))
        })
        .collect::<Result<Vec<_>>>()?;
    let staged_path = staging.keep();
    replace_directory(&staged_path, &args.out)?;
    cairn_uqa::config::atomic_write(&args.stats, &serde_json::to_vec_pretty(&report.stats)?)?;
    match format {
        OutputFormat::Json => print_json(&serde_json::json!({
            "input_chunks": report.input_chunks,
            "tombstoned": report.tombstoned,
            "output_chunks": report.output_chunks,
            "shards": final_shards.iter().map(|(path, r)| serde_json::json!({"path": path, "report": r})).collect::<Vec<_>>(),
            "stats": args.stats,
        })),
        OutputFormat::Human => {
            println!(
                "✓ compacted {} → {} live chunks ({} tombstoned)",
                report.input_chunks, report.output_chunks, report.tombstoned
            );
            for (path, r) in final_shards {
                println!(
                    "  {}  docs={} bytes={}",
                    path.display(),
                    r.documents,
                    r.output_bytes
                );
            }
            println!("✓ {}  revision stats", args.stats.display());
            Ok(())
        }
    }
}

async fn publish(
    config: &CairnConfig,
    overrides: &StoreOverrides,
    args: PublishArgs,
    format: OutputFormat,
) -> Result<()> {
    publish_with_expectation(config, overrides, args, format, ParentExpectation::Auto).await
}

async fn publish_with_expectation(
    config: &CairnConfig,
    overrides: &StoreOverrides,
    args: PublishArgs,
    format: OutputFormat,
    parent_expectation: ParentExpectation,
) -> Result<()> {
    let (tenant, kb) = resolve_scope(config, args.scope)?;
    let store = config.resolve_store(overrides)?;
    let catalog = Catalog::new(store.clone());
    let current = catalog.get_head(&tenant, &kb).await?;
    let current_revision = current.as_ref().map(|(head, _)| head.revision);

    enforce_parent_expectation(current_revision, parent_expectation)?;

    let requested_revision = args.revision;
    let existing_same_revision = match (requested_revision, current.as_ref()) {
        (Some(requested), Some((head, _))) if requested == head.revision => {
            Some(catalog.resolve(&tenant, &kb, Some(requested)).await?)
        }
        _ => None,
    };

    let retry_parent = existing_same_revision
        .as_ref()
        .map(|manifest| manifest.parent_revision);
    let parent = select_parent(
        args.parent,
        current_revision,
        retry_parent,
        parent_expectation,
    );
    let revision = match requested_revision {
        Some(revision) => revision,
        None => {
            catalog
                .next_available_revision(&tenant, &kb, parent)
                .await?
        }
    };

    if let Some((head, _)) = current.as_ref() {
        if revision != head.revision && parent != Some(head.revision) {
            bail!(
                "parent {} is stale; current HEAD is {}",
                parent.map_or_else(|| "<none>".to_owned(), |value| value.to_string()),
                head.revision,
            );
        }
    } else if parent.is_some() {
        bail!("cannot specify a parent for a knowledge base without HEAD");
    }

    let parent_manifest = match parent {
        Some(parent_revision) => Some(catalog.resolve(&tenant, &kb, Some(parent_revision)).await?),
        None => None,
    };
    let local_shards = collect_shards(args.shards, args.shard_dir.as_deref())?;
    let inferred_dimension = infer_dimension(&local_shards)?;
    let dimension = args
        .dimension
        .or(inferred_dimension)
        .or_else(|| parent_manifest.as_ref().map(|m| m.dimension))
        .context("cannot infer vector dimension; pass --dimension or provide a local shard")?;
    let embedding_provider = args
        .embedding_provider
        .map(|provider| provider.as_str().to_owned())
        .or_else(|| {
            parent_manifest
                .as_ref()
                .map(|manifest| manifest.embedding_provider.clone())
        })
        .unwrap_or_else(|| config.embedding.provider.clone());
    let embedding_model = args
        .embedding_model
        .or_else(|| parent_manifest.as_ref().map(|m| m.embedding_model.clone()))
        .or_else(|| config.defaults.embedding_model.clone())
        .unwrap_or_else(|| config.embedding.model.clone());
    if let Some(parent_manifest) = &parent_manifest {
        if !args.replace_shards && parent_manifest.dimension != dimension {
            bail!("delta revision dimension {dimension} differs from parent dimension {}; use a full replacement after re-embedding", parent_manifest.dimension);
        }
        if !args.replace_shards
            && (parent_manifest.embedding_model != embedding_model
                || parent_manifest.embedding_provider != embedding_provider)
        {
            bail!("delta revision embedding provider/model differs from parent; use --replace-shards for a full model migration");
        }
    }
    let stats: RevisionStats = serde_json::from_slice(
        &std::fs::read(&args.stats).with_context(|| format!("read {}", args.stats.display()))?,
    )?;
    stats.validate()?;
    if stats.live_document_count == 0 {
        bail!("revision stats cannot describe an empty corpus")
    }
    validate_local_shards(&local_shards, dimension, &stats.analyzer)?;

    let inherited_shards = if args.replace_shards {
        0
    } else {
        parent_manifest.as_ref().map_or(0, |m| m.shards.len())
    };
    if inherited_shards + local_shards.len() == 0 {
        bail!("revision must contain at least one shard; provide --shard/--shard-dir or inherit a parent shard")
    }
    let tombstone_ids = args
        .tombstones
        .as_ref()
        .map(|path| cairn_uqa::compaction::read_tombstone_lines(path))
        .transpose()?;
    if let Some(path) = args.uqa_db.as_ref() {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("inspect UQA database {}", path.display()))?;
        if !metadata.is_file() || metadata.len() == 0 {
            bail!(
                "--uqa-db must point to a non-empty regular file: {}",
                path.display()
            )
        }
    }

    let plan = PublishPlan {
        tenant: tenant.clone(),
        knowledge_base: kb.clone(),
        revision,
        parent_revision: parent,
        embedding_provider: embedding_provider.clone(),
        embedding_model: embedding_model.clone(),
        dimension,
        local_shards: local_shards
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        inherited_shards,
        replace_shards: args.replace_shards,
        drop_inherited_tombstones: args.drop_inherited_tombstones,
        dry_run: args.dry_run,
    };
    if args.dry_run {
        return show_plan(&plan, format);
    }

    let mut shards = if args.replace_shards {
        Vec::new()
    } else {
        parent_manifest
            .as_ref()
            .map(|m| m.shards.clone())
            .unwrap_or_default()
    };
    for path in &local_shards {
        shards.push(
            upload_content_addressed_file(store.clone(), "objects/shards", path, "cairn").await?,
        );
    }
    if shards.is_empty() {
        bail!("revision must contain at least one shard")
    }

    let mut tombstones = if args.drop_inherited_tombstones {
        Vec::new()
    } else {
        parent_manifest
            .as_ref()
            .map(|m| m.tombstones.clone())
            .unwrap_or_default()
    };
    if let Some(ids) = tombstone_ids {
        if !ids.is_empty() {
            tombstones.push(
                upload_tombstones(store.clone(), "objects/tombstones", ids.into_iter()).await?,
            );
        }
    }
    let stats_descriptor = upload_revision_stats(store.clone(), "objects/stats", &stats).await?;
    let uqa_bundle = match args.uqa_db.as_ref() {
        Some(path) => Some(
            upload_uqa_bundle_file(
                store.clone(),
                "objects/uqa",
                path,
                revision,
                "knowledge.uqa",
            )
            .await?,
        ),
        None => None,
    };
    let manifest = RevisionManifest {
        tenant,
        knowledge_base: kb,
        revision,
        parent_revision: parent,
        created_at_unix_ms: existing_same_revision
            .as_ref()
            .map_or_else(now_unix_ms, |manifest| Ok(manifest.created_at_unix_ms))?,
        embedding_provider,
        embedding_model,
        dimension,
        shards,
        stats: stats_descriptor,
        tombstones,
        uqa_bundle,
    };
    catalog.publish(&manifest).await?;
    match format {
        OutputFormat::Json => print_json(&manifest),
        OutputFormat::Human => {
            println!("✓ published revision {}", manifest.revision);
            println!(
                "  {}/{}  shards={} tombstone_sets={}",
                manifest.tenant,
                manifest.knowledge_base,
                manifest.shards.len(),
                manifest.tombstones.len()
            );
            println!(
                "  embedding={}/{} dim={}",
                manifest.embedding_provider, manifest.embedding_model, manifest.dimension
            );
            Ok(())
        }
    }
}

async fn head(
    config: &CairnConfig,
    overrides: &StoreOverrides,
    scope: ScopeArgs,
    format: OutputFormat,
) -> Result<()> {
    let (tenant, kb) = resolve_scope(config, scope)?;
    let catalog = Catalog::new(config.resolve_store(overrides)?);
    let head = catalog.get_head(&tenant, &kb).await?.map(|(head, _)| head);
    match format {
        OutputFormat::Json => print_json(&head),
        OutputFormat::Human => match head {
            Some(head) => {
                println!("{}", head.revision);
                Ok(())
            }
            None => {
                println!("no HEAD");
                Ok(())
            }
        },
    }
}

async fn promote(
    config: &CairnConfig,
    overrides: &StoreOverrides,
    args: PromoteArgs,
    format: OutputFormat,
) -> Result<()> {
    let (tenant, kb) = resolve_scope(config, args.scope)?;
    let catalog = Catalog::new(config.resolve_store(overrides)?);
    let current = catalog
        .get_head(&tenant, &kb)
        .await?
        .map(|(head, _)| head.revision);
    let target = catalog
        .preflight_promote(&tenant, &kb, args.revision)
        .await?;
    if args.dry_run {
        let value = serde_json::json!({
            "tenant": tenant,
            "kb": kb,
            "from_revision": current,
            "to_revision": target.revision,
            "dry_run": true,
        });
        return match format {
            OutputFormat::Json => print_json(&value),
            OutputFormat::Human => {
                println!(
                    "✓ promotable {tenant}/{kb}: {} → {} (dry run)",
                    current.map_or_else(|| "<none>".to_owned(), |revision| revision.to_string()),
                    target.revision,
                );
                Ok(())
            }
        };
    }
    catalog.promote(&tenant, &kb, target.revision).await?;
    match format {
        OutputFormat::Json => print_json(
            &serde_json::json!({"tenant": tenant, "kb": kb, "revision": target.revision}),
        ),
        OutputFormat::Human => {
            println!("✓ promoted {tenant}/{kb} to revision {}", target.revision);
            Ok(())
        }
    }
}

async fn search(
    config: &CairnConfig,
    overrides: &StoreOverrides,
    args: SearchArgs,
    format: OutputFormat,
) -> Result<()> {
    let (tenant, kb) = resolve_scope(config, args.scope)?;
    if args.query_file.as_deref() == Some(Path::new("-"))
        && args.vector_file.as_deref() == Some(Path::new("-"))
    {
        bail!("stdin can feed either --query-file - or --vector-file -, not both");
    }
    let query = read_query(args.query, args.query_file.as_deref())?;
    let vector = read_vector(args.vector.as_deref(), args.vector_file.as_deref())?;
    let filters = match args.filters {
        Some(raw) => serde_json::from_str::<BTreeMap<String, serde_json::Value>>(&raw)
            .context("--filters must be a JSON object")?,
        None => BTreeMap::new(),
    };
    let request = SearchRequest {
        query,
        query_vector: vector,
        limit: args.limit,
        candidate_limit: args.candidates,
        revision: args.revision,
        filters,
        max_remote_bytes: args.max_remote_bytes,
        max_range_reads: args.max_range_reads,
    };
    request.validate()?;
    if args.nprobe == 0 || args.nprobe > 256 {
        bail!("--nprobe must be in 1..=256")
    }
    let store = config.resolve_store(overrides)?;
    let mut runtime =
        CairnRuntime::new_with_cache_limit(store, config.cache_path(args.cache.as_deref()), None);
    runtime.auto_embed = !args.lexical_only;
    if runtime.auto_embed && request.query_vector.is_empty() && !request.query.trim().is_empty() {
        let manifest = runtime
            .catalog
            .resolve(&tenant, &kb, request.revision)
            .await?;
        if manifest.embedding_provider == "openrouter" {
            runtime = runtime.with_embedder(config.embedding.client()?);
        }
    }
    runtime.nprobe = args.nprobe;
    runtime.limits = RuntimeLimits {
        max_remote_bytes_per_query: args.max_remote_bytes,
        max_range_reads_per_query: args.max_range_reads,
        max_candidate_limit: args
            .candidates
            .max(RuntimeLimits::default().max_candidate_limit),
    }
    .validate()?;
    let response = runtime.search(&tenant, &kb, &request).await?;
    match format {
        OutputFormat::Json => print_json(&response),
        OutputFormat::Human => {
            println!(
                "revision={} mode={:?} approximate={} remote={}B reads={}",
                response.revision,
                response.mode,
                response.approximate,
                response.remote_bytes,
                response.range_reads
            );
            for (rank, hit) in response.hits.iter().enumerate() {
                let preview: String = hit.text.chars().take(160).collect();
                println!(
                    "{:>3}. {:>9.4}  {}  {}",
                    rank + 1,
                    hit.score,
                    hit.id,
                    preview.replace('\n', " ")
                );
            }
            Ok(())
        }
    }
}

fn uqa_build(args: UqaBuildArgs, format: OutputFormat) -> Result<()> {
    let options = cairn_uqa::uqa::UqaBuildOptions {
        revision: args.revision,
        analyzer: args.analyzer,
        hnsw_m: args.hnsw_m,
        ef_construction: args.ef_construction,
        ef_search: args.ef_search,
        replace_existing: args.force,
    };
    cairn_uqa::uqa::build_uqa_from_jsonl(&args.input, &args.out, &options)?;
    match format {
        OutputFormat::Json => {
            print_json(&serde_json::json!({"path": args.out, "revision": args.revision}))
        }
        OutputFormat::Human => {
            println!("✓ built warm UQA database {}", args.out.display());
            Ok(())
        }
    }
}

async fn serve_command(
    config: &CairnConfig,
    overrides: &StoreOverrides,
    args: ServeArgs,
) -> Result<()> {
    if args.nprobe == 0 || args.nprobe > 256 {
        bail!("--nprobe must be in 1..=256")
    }
    let store = config.resolve_store(overrides)?;
    let mut runtime = CairnRuntime::new_with_cache_limit(
        store,
        config.cache_path(args.cache.as_deref()),
        args.cache_max_bytes,
    );
    runtime.auto_embed = !args.lexical_only;
    if runtime.auto_embed {
        if let Some(embedder) = config.embedding.client_if_configured()? {
            runtime = runtime.with_embedder(embedder);
        } else {
            tracing::warn!(
                env = %config.embedding.api_key_env,
                "OpenRouter API key is not configured; server will still start, but text-only queries against OpenRouter revisions will require a query_vector or will return an explicit error"
            );
        }
    }
    runtime.nprobe = args.nprobe;
    runtime.limits = RuntimeLimits {
        max_remote_bytes_per_query: args.max_remote_bytes_per_query,
        max_range_reads_per_query: args.max_range_reads_per_query,
        max_candidate_limit: args.max_candidate_limit,
    }
    .validate()?;
    validate_env_name(&args.auth_token_env)?;
    let bearer_token = std::env::var(&args.auth_token_env)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if bearer_token.is_none() && args.listen.ip().is_loopback() {
        tracing::warn!(
            env = %args.auth_token_env,
            "CAIRN server token is not configured; loopback KB endpoints are unauthenticated",
        );
    }
    let allowed_scope = if args.restrict_to_default_scope {
        let (tenant, knowledge_base) = resolve_scope(
            config,
            ScopeArgs {
                tenant: None,
                kb: None,
            },
        )?;
        Some(ServerScope {
            tenant,
            knowledge_base,
        })
    } else {
        match (args.allowed_tenant, args.allowed_kb) {
            (Some(tenant), Some(knowledge_base)) => Some(ServerScope {
                tenant,
                knowledge_base,
            }),
            (None, None) => None,
            _ => bail!("--allowed-tenant and --allowed-kb must be supplied together"),
        }
    };
    serve(
        runtime,
        ServerOptions {
            listen: args.listen,
            allow_historical_revisions: args.allow_historical_revisions,
            bearer_token,
            allowed_scope,
            allow_unauthenticated_non_loopback: args.allow_unauthenticated,
            ..ServerOptions::default()
        },
    )
    .await
}

fn inspect_shard(path: &Path, format: OutputFormat) -> Result<()> {
    let meta = read_shard_meta(path)?;
    match format {
        OutputFormat::Json => print_json(&meta),
        OutputFormat::Human => {
            println!("{}", path.display());
            println!(
                "  format={} docs={} dim={} ivf_lists={} terms={} analyzer={}",
                meta.format_version,
                meta.document_count,
                meta.dimension,
                meta.ivf_lists,
                meta.term_to_block.len(),
                meta.analyzer
            );
            Ok(())
        }
    }
}

fn completions(shell: CompletionShell) -> Result<()> {
    let mut command = Cli::command();
    let mut stdout = std::io::stdout();
    let shell = clap_complete::Shell::from(shell);
    clap_complete::generate(shell, &mut command, "cairn", &mut stdout);
    Ok(())
}

fn resolve_scope(config: &CairnConfig, args: ScopeArgs) -> Result<(String, String)> {
    resolve_scope_optional(config, args)?.context("tenant and knowledge base are required; pass --tenant/--kb, set CAIRN_TENANT/CAIRN_KB, or configure defaults")
}

fn resolve_scope_optional(
    config: &CairnConfig,
    args: ScopeArgs,
) -> Result<Option<(String, String)>> {
    let tenant = config.tenant(args.tenant.as_deref()).map(str::to_owned);
    let kb = config.knowledge_base(args.kb.as_deref()).map(str::to_owned);
    match (tenant, kb) {
        (Some(tenant), Some(kb)) => {
            cairn_uqa::manifest::validate_scope_component("tenant", &tenant)?;
            cairn_uqa::manifest::validate_scope_component("knowledge_base", &kb)?;
            Ok(Some((tenant, kb)))
        }
        (None, None) => Ok(None),
        _ => bail!("tenant and knowledge base must be provided together"),
    }
}

fn collect_shards(mut explicit: Vec<PathBuf>, directory: Option<&Path>) -> Result<Vec<PathBuf>> {
    if let Some(directory) = directory {
        let discovered = std::fs::read_dir(directory)
            .with_context(|| format!("read shard directory {}", directory.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("enumerate shard directory {}", directory.display()))?;
        explicit.extend(
            discovered
                .into_iter()
                .filter(|path| path.extension().and_then(|x| x.to_str()) == Some("cairn")),
        );
    }
    let mut shards = explicit
        .into_iter()
        .map(|path| {
            std::fs::canonicalize(&path)
                .with_context(|| format!("resolve shard {}", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    shards.sort();
    shards.dedup();
    Ok(shards)
}

fn infer_dimension(shards: &[PathBuf]) -> Result<Option<u32>> {
    let mut dimension = None;
    for path in shards {
        let meta =
            read_shard_meta(path).with_context(|| format!("validate shard {}", path.display()))?;
        match dimension {
            None => dimension = Some(meta.dimension),
            Some(expected) if expected == meta.dimension => {}
            Some(expected) => bail!(
                "shard dimension mismatch: expected {expected}, {} has {}",
                path.display(),
                meta.dimension
            ),
        }
    }
    Ok(dimension)
}

fn validate_local_shards(shards: &[PathBuf], dimension: u32, analyzer: &str) -> Result<()> {
    shards.iter().try_for_each(|path| {
        let meta =
            read_shard_meta(path).with_context(|| format!("validate shard {}", path.display()))?;
        if meta.dimension != dimension {
            bail!(
                "shard dimension mismatch: revision expects {dimension}, {} has {}",
                path.display(),
                meta.dimension
            )
        }
        if meta.analyzer != analyzer {
            bail!(
                "shard analyzer mismatch: revision stats use {analyzer:?}, {} uses {:?}",
                path.display(),
                meta.analyzer
            )
        }
        Ok(())
    })
}

fn read_scoring(path: Option<&Path>, allow_dev_defaults: bool) -> Result<GlobalScoringContext> {
    match path {
        Some(path) => serde_json::from_slice(&std::fs::read(path).with_context(|| format!("read scoring config {}", path.display()))?)
            .context("parse scoring config"),
        None if allow_dev_defaults => {
            eprintln!("warning: using development calibration defaults; posterior values are not production-calibrated");
            Ok(GlobalScoringContext::default())
        },
        None => bail!("--scoring is required when writing revision stats; use --dev-calibration only for local development"),
    }
}

fn read_query(inline: Option<String>, file: Option<&Path>) -> Result<String> {
    match (inline, file) {
        (Some(query), None) => Ok(query),
        (None, Some(path)) => read_bounded_text(path, 16 * 1024, "query"),
        (None, None) => Ok(String::new()),
        (Some(_), Some(_)) => bail!("use either a positional query or --query-file"),
    }
}

fn read_vector(inline: Option<&str>, file: Option<&Path>) -> Result<Vec<f32>> {
    let raw = match (inline, file) {
        (Some(raw), None) => Some(raw.to_owned()),
        (None, Some(path)) => Some(read_bounded_text(path, 4 * 1024 * 1024, "vector")?),
        (None, None) => None,
        (Some(_), Some(_)) => bail!("use either --vector or --vector-file"),
    };
    raw.map_or_else(
        || Ok(Vec::new()),
        |raw| {
            serde_json::from_str::<Vec<f32>>(&raw).context("vector must be a JSON array of numbers")
        },
    )
}

fn read_bounded_text(path: &Path, max_bytes: usize, label: &str) -> Result<String> {
    use std::io::Read as _;

    let max_plus_one = u64::try_from(max_bytes)
        .context("input size limit exceeds u64")?
        .checked_add(1)
        .context("input size limit overflow")?;
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    if path == Path::new("-") {
        std::io::stdin()
            .lock()
            .take(max_plus_one)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read {label} from stdin"))?;
    } else {
        std::fs::File::open(path)
            .with_context(|| format!("open {label} {}", path.display()))?
            .take(max_plus_one)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read {label} {}", path.display()))?;
    }
    if bytes.len() > max_bytes {
        bail!("{label} input exceeds {max_bytes} bytes");
    }
    String::from_utf8(bytes).with_context(|| format!("{label} input must be UTF-8"))
}

fn enforce_parent_expectation(current: Option<u64>, expectation: ParentExpectation) -> Result<()> {
    if let ParentExpectation::Exact(expected) = expectation {
        if current != expected {
            bail!(
                "HEAD changed while artifacts were being built: expected parent {}, found {}; rebuild/retry against the new HEAD",
                expected.map_or_else(|| "<root>".to_owned(), |value| value.to_string()),
                current.map_or_else(|| "<root>".to_owned(), |value| value.to_string()),
            )
        }
    }
    Ok(())
}

#[must_use]
fn select_parent(
    explicit: Option<u64>,
    current: Option<u64>,
    retry_parent: Option<Option<u64>>,
    expectation: ParentExpectation,
) -> Option<u64> {
    match expectation {
        ParentExpectation::Exact(expected) => expected,
        ParentExpectation::Auto => explicit.or_else(|| retry_parent.unwrap_or(current)),
    }
}

fn now_unix_ms() -> Result<u64> {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
        .context("system time exceeds u64 milliseconds")
}

fn show_plan(plan: &PublishPlan, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => print_json(plan),
        OutputFormat::Human => {
            println!("Publish plan (dry run)");
            println!(
                "  target: {}/{} revision {}",
                plan.tenant, plan.knowledge_base, plan.revision
            );
            println!(
                "  parent: {}",
                plan.parent_revision
                    .map_or_else(|| "<root>".to_owned(), |r| r.to_string())
            );
            println!(
                "  embedding: {}/{} dim={}",
                plan.embedding_provider, plan.embedding_model, plan.dimension
            );
            println!(
                "  new shards: {}  inherited shards: {}",
                plan.local_shards.len(),
                plan.inherited_shards
            );
            for shard in &plan.local_shards {
                println!("    + {shard}");
            }
            Ok(())
        }
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

async fn prepare_snapshot_chunks(
    config: &CairnConfig,
    input: &Path,
    model_override: Option<&str>,
    dimensions_override: Option<u32>,
    provider_override: Option<EmbeddingProviderArg>,
    reembed: bool,
    no_embed: bool,
) -> Result<(Vec<ChunkInput>, String, String)> {
    let mut chunks = read_jsonl_unvalidated(input)?;
    if reembed {
        chunks.iter_mut().for_each(|chunk| chunk.vector.clear());
    }

    let existing_dimensions = chunks
        .iter()
        .filter_map(|chunk| (!chunk.vector.is_empty()).then_some(chunk.vector.len()))
        .collect::<BTreeSet<_>>();
    if existing_dimensions.len() > 1 {
        bail!("precomputed input vectors have inconsistent dimensions")
    }
    let existing_dimension = existing_dimensions
        .iter()
        .next()
        .copied()
        .map(|dimension| u32::try_from(dimension).context("input vector dimension exceeds u32"))
        .transpose()?;
    if let (Some(requested), Some(existing)) = (dimensions_override, existing_dimension) {
        if requested != existing {
            bail!("--embedding-dimensions {requested} conflicts with precomputed input vector dimension {existing}; use --reembed to replace them")
        }
    }

    let dimensions = dimensions_override
        .or(existing_dimension)
        .unwrap_or(config.embedding.dimensions);
    if dimensions == 0 || dimensions > 65_536 {
        bail!("embedding dimensions must be in 1..=65536")
    }
    let model = model_override
        .map(str::to_owned)
        .or_else(|| config.defaults.embedding_model.clone())
        .unwrap_or_else(|| config.embedding.model.clone());

    let missing = chunks
        .iter()
        .enumerate()
        .filter_map(|(index, chunk)| chunk.vector.is_empty().then_some(index))
        .collect::<Vec<_>>();

    if !missing.is_empty()
        && existing_dimension.is_some()
        && !reembed
        && provider_override.is_none()
    {
        bail!(
            "input mixes precomputed and missing vectors; use --reembed to create one consistent OpenRouter vector space, or pass --embedding-provider openrouter to assert the existing vectors came from the declared OpenRouter model"
        )
    }

    let provider = match provider_override {
        Some(provider) => provider.as_str().to_owned(),
        None if missing.is_empty() => "external".to_owned(),
        None => "openrouter".to_owned(),
    };
    if missing.is_empty() && provider_override.is_none() {
        eprintln!(
            "→ precomputed vectors detected; marking provenance as external (pass --embedding-provider openrouter if they were produced by the declared OpenRouter model)"
        );
    }

    if !missing.is_empty() {
        if no_embed {
            bail!(
                "{} chunks have no vector; remove --no-embed or provide vectors",
                missing.len()
            )
        }
        if provider != "openrouter" {
            bail!("missing vectors require the OpenRouter embedding provider")
        }
        let embedder = config.embedding.client()?;
        let texts = missing
            .iter()
            .map(|&index| chunks[index].text.as_str())
            .collect::<Vec<_>>();
        eprintln!(
            "→ embedding {} chunks with OpenRouter model {} ({}d)",
            texts.len(),
            model,
            dimensions
        );
        let vectors = embedder
            .embed(&model, dimensions, EmbeddingInputType::Document, &texts)
            .await?;
        if vectors.len() != missing.len() {
            bail!("embedding provider returned an unexpected number of vectors")
        }
        for (index, vector) in missing.into_iter().zip(vectors) {
            chunks[index].vector = vector;
        }
    }

    validate_chunks(&chunks)?;
    Ok((chunks, provider, model))
}

#[cfg(test)]
mod cli_unit_tests {
    use super::{enforce_parent_expectation, select_parent, ParentExpectation};

    #[test]
    fn snapshot_root_expectation_does_not_adopt_concurrent_head() {
        assert!(enforce_parent_expectation(Some(1), ParentExpectation::Exact(None)).is_err());
        assert!(enforce_parent_expectation(None, ParentExpectation::Exact(None)).is_ok());
    }

    #[test]
    fn same_revision_retry_preserves_original_root_parent() {
        assert_eq!(
            select_parent(None, Some(1), Some(None), ParentExpectation::Auto),
            None,
        );
        assert_eq!(
            select_parent(None, Some(2), Some(Some(1)), ParentExpectation::Auto),
            Some(1),
        );
    }
}
