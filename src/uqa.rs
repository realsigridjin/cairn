use crate::model::{RevisionStats, SearchHit, SearchRequest};
#[cfg(feature = "uqa")]
use crate::search::fusion::Evidence;
#[cfg(feature = "uqa")]
use anyhow::Context;
use anyhow::{bail, Result};
#[cfg(feature = "uqa")]
use std::collections::BTreeMap;
use std::path::Path;

#[must_use]
pub const fn warm_execution_enabled() -> bool {
    cfg!(feature = "uqa")
}

#[cfg(feature = "uqa")]
use std::path::PathBuf;

#[cfg(feature = "uqa")]
pub async fn search_uqa(
    path: &Path,
    req: &SearchRequest,
    stats: &RevisionStats,
    expected_revision: u64,
    expected_dimension: u32,
) -> Result<Vec<SearchHit>> {
    let path = path.to_path_buf();
    let req = req.clone();
    let stats = stats.clone();
    tokio::task::spawn_blocking(move || {
        search_uqa_blocking(path, &req, &stats, expected_revision, expected_dimension)
    })
    .await?
}

#[cfg(feature = "uqa")]
fn search_uqa_blocking(
    path: PathBuf,
    req: &SearchRequest,
    stats: &RevisionStats,
    expected_revision: u64,
    expected_dimension: u32,
) -> Result<Vec<SearchHit>> {
    use uqa_core::Value;
    use uqa_engine::{Engine, SQLParam};

    #[derive(Default)]
    struct Candidate {
        text: String,
        metadata: BTreeMap<String, serde_json::Value>,
        lexical_raw: Option<f32>,
        vector_raw: Option<f32>,
    }

    let engine =
        Engine::open(&path).with_context(|| format!("open UQA database {}", path.display()))?;
    validate_materialized_database(&engine, stats, expected_revision, expected_dimension)?;
    let candidate_limit = req.effective_candidate_limit();
    let mut candidates: BTreeMap<String, Candidate> = BTreeMap::new();

    if !req.query.trim().is_empty() {
        let sql = format!(
            "SELECT id, body, metadata_json, _score FROM chunks \
             WHERE text_match(body, $1) ORDER BY _score DESC, id ASC LIMIT {candidate_limit}"
        );
        let result = engine.sql(&sql, &[SQLParam::scalar(Value::Str(req.query.clone()))])?;
        for row in result.rows {
            let id = value_string(row.get("id").context("UQA lexical row missing id")?, "id")?;
            let text = value_string(
                row.get("body").context("UQA lexical row missing body")?,
                "body",
            )?;
            let metadata = parse_metadata(row.get("metadata_json"))?;
            let score = value_f32(
                row.get("_score")
                    .context("UQA lexical row missing _score")?,
                "_score",
            )?;
            if score < 0.0 {
                bail!("UQA BM25 score must be non-negative, got {score}")
            }
            let c = candidates.entry(id).or_default();
            c.text = text;
            c.metadata = metadata;
            c.lexical_raw = Some(score);
        }
    }

    if !req.query_vector.is_empty() {
        let sql = format!(
            "SELECT id, body, metadata_json, _score FROM chunks \
             WHERE knn_match(embedding, $1, {candidate_limit}) ORDER BY _score DESC, id ASC LIMIT {candidate_limit}"
        );
        let result = engine.sql(&sql, &[SQLParam::vector(req.query_vector.clone())])?;
        for row in result.rows {
            let id = value_string(row.get("id").context("UQA vector row missing id")?, "id")?;
            let text = value_string(
                row.get("body").context("UQA vector row missing body")?,
                "body",
            )?;
            let metadata = parse_metadata(row.get("metadata_json"))?;
            let score = value_f32(
                row.get("_score").context("UQA vector row missing _score")?,
                "_score",
            )?
            .clamp(-1.0, 1.0);
            let c = candidates.entry(id).or_default();
            c.text = text;
            c.metadata = metadata;
            c.vector_raw = Some(score);
        }
    }

    let mut hits = Vec::new();
    for (id, c) in candidates {
        if !metadata_matches(&c.metadata, &req.filters) {
            continue;
        }
        let mut evidence = Evidence::default();
        if let Some(raw) = c.lexical_raw {
            evidence.lexical = stats
                .scoring
                .warm
                .lexical
                .evidence_llr(raw, stats.scoring.base_rate);
            evidence.has_lexical = true;
        }
        if let Some(raw) = c.vector_raw {
            evidence.vector = stats
                .scoring
                .warm
                .vector
                .evidence_llr(raw, stats.scoring.base_rate);
            evidence.has_vector = true;
        }
        let score = evidence.fused_logit(&stats.scoring);
        hits.push(SearchHit {
            id,
            score,
            posterior: evidence.posterior(&stats.scoring),
            lexical_evidence: evidence.lexical,
            vector_evidence: evidence.vector,
            text: c.text,
            metadata: c.metadata,
        });
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    hits.truncate(req.limit);
    Ok(hits)
}

#[cfg(feature = "uqa")]
fn validate_materialized_database(
    engine: &uqa_engine::Engine,
    stats: &RevisionStats,
    expected_revision: u64,
    expected_dimension: u32,
) -> Result<()> {
    let result = engine
        .sql("SELECT k, v FROM cairn_meta ORDER BY k ASC", &[])
        .context("UQA bundle is missing CAIRN materialization metadata")?;
    let mut meta = BTreeMap::new();
    for row in result.rows {
        let k = value_string(row.get("k").context("cairn_meta row missing k")?, "k")?;
        let v = value_string(row.get("v").context("cairn_meta row missing v")?, "v")?;
        meta.insert(k, v);
    }
    if meta_required(&meta, "schema_version")? != "1" {
        bail!("unsupported UQA CAIRN metadata schema")
    }
    if meta_required(&meta, "revision")?.parse::<u64>()? != expected_revision {
        bail!("UQA bundle revision mismatch")
    }
    if meta_required(&meta, "dimension")?.parse::<u32>()? != expected_dimension {
        bail!("UQA bundle dimension mismatch")
    }
    if meta_required(&meta, "document_count")?.parse::<u64>()? != stats.live_document_count {
        bail!("UQA bundle document count does not match revision stats")
    }
    if meta_required(&meta, "analyzer")? != stats.warm_analyzer {
        bail!("UQA bundle analyzer does not match revision stats warm_analyzer")
    }
    if meta_required(&meta, "corpus_sha256")? != stats.corpus_sha256 {
        bail!("UQA bundle corpus digest does not match revision stats")
    }
    Ok(())
}

#[cfg(feature = "uqa")]
fn meta_required<'a>(meta: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str> {
    meta.get(key)
        .map(String::as_str)
        .with_context(|| format!("cairn_meta missing {key}"))
}

#[cfg(feature = "uqa")]
fn value_string(value: &uqa_core::Value, name: &str) -> Result<String> {
    match value {
        uqa_core::Value::Str(v) => Ok(v.clone()),
        other => bail!("UQA {name} expected string, got {other:?}"),
    }
}

#[cfg(feature = "uqa")]
fn value_f32(value: &uqa_core::Value, name: &str) -> Result<f32> {
    let value = match value {
        uqa_core::Value::Float(v) => *v as f32,
        uqa_core::Value::Int(v) => *v as f32,
        other => bail!("UQA {name} expected numeric, got {other:?}"),
    };
    if !value.is_finite() {
        bail!("UQA {name} is non-finite")
    }
    Ok(value)
}

#[cfg(feature = "uqa")]
fn parse_metadata(value: Option<&uqa_core::Value>) -> Result<BTreeMap<String, serde_json::Value>> {
    match value {
        None => Ok(BTreeMap::new()),
        Some(uqa_core::Value::Str(v)) => Ok(serde_json::from_str(v)?),
        Some(other) => bail!("UQA metadata_json expected string, got {other:?}"),
    }
}

#[cfg(feature = "uqa")]
fn metadata_matches(
    metadata: &BTreeMap<String, serde_json::Value>,
    filters: &BTreeMap<String, serde_json::Value>,
) -> bool {
    filters.iter().all(|(k, v)| metadata.get(k) == Some(v))
}

#[cfg(not(feature = "uqa"))]
pub async fn search_uqa(
    _path: &Path,
    _req: &SearchRequest,
    _stats: &RevisionStats,
    _expected_revision: u64,
    _expected_dimension: u32,
) -> Result<Vec<SearchHit>> {
    bail!("built without uqa feature")
}

#[derive(Debug, Clone)]
pub struct UqaBuildOptions {
    pub revision: u64,
    pub analyzer: String,
    pub hnsw_m: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
    pub replace_existing: bool,
}

impl Default for UqaBuildOptions {
    fn default() -> Self {
        Self {
            revision: 1,
            analyzer: "standard_cjk".to_owned(),
            hnsw_m: 16,
            ef_construction: 200,
            ef_search: 128,
            replace_existing: false,
        }
    }
}

impl UqaBuildOptions {
    pub fn validate(&self) -> Result<()> {
        if self.revision == 0 {
            bail!("UQA materialization revision must be positive")
        }
        if !matches!(self.analyzer.as_str(), "standard" | "standard_cjk") {
            bail!("UQA analyzer must be 'standard' or 'standard_cjk'")
        }
        if self.hnsw_m == 0 || self.hnsw_m > 256 {
            bail!("hnsw_m must be in 1..=256")
        }
        if self.ef_construction == 0 || self.ef_construction > 100_000 {
            bail!("ef_construction must be in 1..=100000")
        }
        if self.ef_search == 0 || self.ef_search > 100_000 {
            bail!("ef_search must be in 1..=100000")
        }
        Ok(())
    }
}

#[cfg(feature = "uqa")]
pub fn build_uqa_from_jsonl(input: &Path, output: &Path, options: &UqaBuildOptions) -> Result<()> {
    use crate::index::builder::{corpus_sha256, read_jsonl};
    use uqa_core::Value;
    use uqa_engine::{Engine, SQLParam};

    options.validate()?;
    let chunks = read_jsonl(input)?;
    let dim = chunks
        .first()
        .context("input corpus is empty")?
        .vector
        .len();
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let document_count = chunks.len();
    let corpus_sha256 = corpus_sha256(&chunks)?;
    let temporary = tempfile::Builder::new()
        .prefix(".cairn-uqa-build-")
        .tempfile_in(parent)?;
    let temp_path = temporary.into_temp_path();
    let database_path = temp_path.to_path_buf();
    let engine = Engine::open(&database_path)?;
    engine.sql(
        "CREATE TABLE cairn_meta (k TEXT PRIMARY KEY, v TEXT NOT NULL)",
        &[],
    )?;
    for (key, value) in [
        ("schema_version", "1".to_owned()),
        ("revision", options.revision.to_string()),
        ("dimension", dim.to_string()),
        ("document_count", document_count.to_string()),
        ("analyzer", options.analyzer.clone()),
        ("corpus_sha256", corpus_sha256),
    ] {
        engine.sql(
            "INSERT INTO cairn_meta (k, v) VALUES ($1, $2)",
            &[
                SQLParam::scalar(Value::Str(key.to_owned())),
                SQLParam::scalar(Value::Str(value)),
            ],
        )?;
    }
    engine.sql(&format!("CREATE TABLE chunks (id TEXT PRIMARY KEY, body TEXT NOT NULL, metadata_json TEXT NOT NULL, embedding VECTOR({dim}) NOT NULL)"), &[])?;
    engine.sql(
        "CREATE INDEX chunks_body_gin ON chunks USING gin (body)",
        &[],
    )?;
    engine.sql(
        &format!(
            "SELECT * FROM set_table_analyzer('chunks', 'body', '{}', 'both')",
            options.analyzer
        ),
        &[],
    )?;
    let metadata_json = chunks
        .iter()
        .map(|chunk| serde_json::to_string(&chunk.metadata))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    engine.transaction(|transaction| {
        for (chunk, metadata) in chunks.iter().zip(metadata_json.iter()) {
            transaction.sql(
                "INSERT INTO chunks (id, body, metadata_json, embedding) VALUES ($1, $2, $3, $4)",
                &[
                    SQLParam::scalar(Value::Str(chunk.id.clone())),
                    SQLParam::scalar(Value::Str(chunk.text.clone())),
                    SQLParam::scalar(Value::Str(metadata.clone())),
                    SQLParam::vector(chunk.vector.clone()),
                ],
            )?;
        }
        Ok(())
    })?;
    engine.sql(&format!(
        "CREATE INDEX chunks_embedding_hnsw ON chunks USING hnsw (embedding) WITH (m = {}, ef_construction = {}, ef_search = {}, seed = 42)",
        options.hnsw_m, options.ef_construction, options.ef_search
    ), &[])?;
    drop(engine);
    std::fs::File::open(&database_path)?.sync_all()?;

    // Keep the TempPath guard alive until the final rename so every error before
    // this point automatically removes the staged database.
    if output.exists() && !options.replace_existing {
        bail!(
            "UQA output already exists: {}; pass --force to replace it",
            output.display()
        )
    }
    if output.exists() {
        let backup = parent.join(format!(".cairn-uqa-old-{}", uuid::Uuid::new_v4()));
        std::fs::rename(output, &backup)?;
        match std::fs::rename(&database_path, output) {
            Ok(()) => {
                crate::config::sync_directory(parent)?;
                if let Err(error) = std::fs::remove_file(&backup) {
                    tracing::warn!(path = %backup.display(), %error, "UQA build succeeded but old database cleanup failed");
                } else {
                    crate::config::sync_directory(parent)?;
                }
            },
            Err(install_error) => {
                match std::fs::rename(&backup, output) {
                    Ok(()) => {
                        crate::config::sync_directory(parent)?;
                        return Err(install_error.into());
                    },
                    Err(restore_error) => bail!(
                        "failed to install UQA database {} ({install_error}) and failed to restore backup {} ({restore_error}); staged database remains at {}",
                        output.display(), backup.display(), database_path.display()
                    ),
                }
            },
        }
    } else {
        std::fs::rename(&database_path, output)?;
        crate::config::sync_directory(parent)?;
    }
    drop(temp_path);
    Ok(())
}

#[cfg(not(feature = "uqa"))]
pub fn build_uqa_from_jsonl(
    _input: &Path,
    _output: &Path,
    _options: &UqaBuildOptions,
) -> Result<()> {
    bail!("UQA materialization requires building CAIRN with --features uqa")
}
