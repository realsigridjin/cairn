use async_trait::async_trait;
use bytes::Bytes;
use cairn_uqa::embedding::{EmbeddingInputType, TextEmbedder};
use cairn_uqa::index::builder::{build_shards, revision_stats_from_chunks, BuildOptions};
use cairn_uqa::manifest::{
    upload_content_addressed, upload_revision_stats, Catalog, RevisionManifest,
};
use cairn_uqa::model::{
    ChunkInput, ChunkLookupOrder, ChunkLookupRequest, GlobalScoringContext, RetrievalMode,
    ScoreDomain, SearchMode, SearchRequest, SearchResponse,
};
use cairn_uqa::object_store::{LocalStore, ObjectMeta, ObjectStore, PutCondition};
use cairn_uqa::CairnRuntime;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Barrier;

fn chunk(id: &str, text: &str, vector: Vec<f32>) -> ChunkInput {
    ChunkInput {
        id: id.into(),
        text: text.into(),
        vector,
        metadata: BTreeMap::new(),
    }
}

fn corpus() -> Vec<ChunkInput> {
    vec![
        chunk("rust-v1", "Rust systems programming", vec![1.0, 0.0]),
        chunk("search", "search engine retrieval", vec![0.0, 1.0]),
        chunk("object", "immutable object storage", vec![0.8, 0.2]),
        chunk("other", "unrelated material", vec![0.0, 1.0]),
    ]
}

fn search_request(query: &str, vector: Vec<f32>) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        query_vector: vector,
        limit: 3,
        candidate_limit: 16,
        revision: None,
        filters: BTreeMap::new(),
        require_text: Vec::new(),
        require_text_case_sensitive: false,
        max_text_bytes: 0,
        max_remote_bytes: 256 * 1024 * 1024,
        max_range_reads: 4096,
    }
}

fn lookup_request(prefix: &str) -> ChunkLookupRequest {
    ChunkLookupRequest {
        id_prefix: prefix.into(),
        limit: 100,
        revision: None,
        order_by: ChunkLookupOrder::Id,
        max_remote_bytes: 256 * 1024 * 1024,
        max_range_reads: 4096,
    }
}

async fn publish_search_fixture(
    store: Arc<dyn ObjectStore>,
    dir: &Path,
    provider: &str,
    shard_count: usize,
) -> anyhow::Result<()> {
    let all = corpus();
    let outputs = build_shards(
        &all,
        &dir.join("built"),
        shard_count,
        &BuildOptions {
            ivf_lists: 2,
            ..BuildOptions::default()
        },
    )?;
    let mut shards = Vec::new();
    for (path, _) in outputs {
        let bytes = Bytes::from(tokio::fs::read(path).await?);
        shards
            .push(upload_content_addressed(store.clone(), "objects/shards", bytes, "cairn").await?);
    }
    let stats = revision_stats_from_chunks(
        &all,
        GlobalScoringContext::default(),
        BuildOptions::default().analyzer,
        "standard_cjk",
    )?;
    let stats = upload_revision_stats(store.clone(), "objects/stats", &stats).await?;
    let manifest = RevisionManifest {
        tenant: "t".into(),
        knowledge_base: "kb".into(),
        revision: 1,
        parent_revision: None,
        created_at_unix_ms: 1,
        embedding_provider: provider.into(),
        embedding_model: "test-model".into(),
        dimension: 2,
        shards,
        stats,
        tombstones: vec![],
        uqa_bundle: None,
    };
    Catalog::new(store.clone()).publish(&manifest).await?;
    Ok(())
}

/// Counting wrapper around `LocalStore`. Publication itself range-reads shard
/// headers/directories for manifest validation, so counting and the optional
/// first-header gate stay disarmed until the fixture is fully published.
#[derive(Clone)]
struct CountingStore {
    inner: LocalStore,
    armed: Arc<AtomicBool>,
    shard_reads: Arc<AtomicU64>,
    shard_bytes: Arc<AtomicU64>,
    header_reads: Arc<AtomicU64>,
    header_gate: Option<Arc<Barrier>>,
}

impl CountingStore {
    fn new(root: impl Into<PathBuf>, header_gate: Option<Arc<Barrier>>) -> Self {
        Self {
            inner: LocalStore::new(root),
            armed: Arc::new(AtomicBool::new(false)),
            shard_reads: Arc::new(AtomicU64::new(0)),
            shard_bytes: Arc::new(AtomicU64::new(0)),
            header_reads: Arc::new(AtomicU64::new(0)),
            header_gate,
        }
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn shard_stats(&self) -> (u64, u64) {
        (
            self.shard_reads.load(Ordering::SeqCst),
            self.shard_bytes.load(Ordering::SeqCst),
        )
    }

    fn header_read_count(&self) -> u64 {
        self.header_reads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ObjectStore for CountingStore {
    async fn head(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>> {
        self.inner.head(key).await
    }

    async fn get_range(&self, key: &str, offset: u64, len: u64) -> anyhow::Result<Bytes> {
        if self.armed.load(Ordering::SeqCst) && key.starts_with("objects/shards") {
            self.shard_reads.fetch_add(1, Ordering::SeqCst);
            if offset == 0 && self.header_reads.fetch_add(1, Ordering::SeqCst) == 0 {
                // Hold the first shard header read at an exact rendezvous so
                // concurrent openers genuinely overlap on the singleflight lock.
                if let Some(gate) = &self.header_gate {
                    gate.wait().await;
                }
            }
        }
        let bytes = self.inner.get_range(key, offset, len).await?;
        if self.armed.load(Ordering::SeqCst) && key.starts_with("objects/shards") {
            self.shard_bytes
                .fetch_add(bytes.len() as u64, Ordering::SeqCst);
        }
        Ok(bytes)
    }

    async fn put(
        &self,
        key: &str,
        bytes: Bytes,
        condition: PutCondition,
    ) -> anyhow::Result<Option<ObjectMeta>> {
        self.inner.put(key, bytes, condition).await
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.inner.delete(key).await
    }

    async fn download_to(&self, key: &str, path: &Path, max_bytes: u64) -> anyhow::Result<u64> {
        self.inner.download_to(key, path, max_bytes).await
    }

    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        condition: PutCondition,
    ) -> anyhow::Result<Option<ObjectMeta>> {
        self.inner.put_file(key, path, condition).await
    }
}

#[derive(Clone, Default)]
struct FakeEmbedder {
    calls: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
}

impl FakeEmbedder {
    fn succeeding() -> Self {
        Self::default()
    }

    fn failing() -> Self {
        let embedder = Self::default();
        embedder.fail.store(true, Ordering::SeqCst);
        embedder
    }

    fn set_succeeding(&self) {
        self.fail.store(false, Ordering::SeqCst);
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TextEmbedder for FakeEmbedder {
    async fn embed(
        &self,
        _model: &str,
        dimensions: u32,
        _input_type: EmbeddingInputType,
        inputs: &[&str],
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("fake embedder outage");
        }
        Ok(inputs
            .iter()
            .map(|_| vec![1.0f32; dimensions as usize])
            .collect())
    }
}

#[test]
fn retrieval_status_serializes_exact_wire_values() -> anyhow::Result<()> {
    assert_eq!(serde_json::to_value(RetrievalMode::Lexical)?, "lexical");
    assert_eq!(serde_json::to_value(RetrievalMode::Hybrid)?, "hybrid");
    assert_eq!(serde_json::to_value(RetrievalMode::Vector)?, "vector");
    for invalid in [
        serde_json::json!("sparse"),
        serde_json::json!("LEXICAL"),
        serde_json::json!(0),
        serde_json::json!(null),
    ] {
        assert!(
            serde_json::from_value::<RetrievalMode>(invalid.clone()).is_err(),
            "unknown retrieval_mode must be rejected by typed serde: {invalid}"
        );
    }

    let response = SearchResponse {
        revision: 7,
        embedding_provider: "external".into(),
        embedding_model: "m".into(),
        dimension: 2,
        corpus_sha256: "ab".repeat(32),
        mode: SearchMode::Cold,
        score_domain: ScoreDomain::RevisionCalibratedLogOdds,
        retrieval_mode: RetrievalMode::Hybrid,
        query_embedding_fallback: true,
        approximate: false,
        hits: vec![],
        remote_bytes: 0,
        range_reads: 0,
    };
    let value = serde_json::to_value(&response)?;
    assert_eq!(value["retrieval_mode"], "hybrid");
    assert_eq!(value["query_embedding_fallback"], true);
    assert!(value.get("lexical_fallback").is_none());
    let decoded: SearchResponse = serde_json::from_value(value)?;
    assert_eq!(decoded.retrieval_mode, RetrievalMode::Hybrid);
    assert!(decoded.query_embedding_fallback);
    Ok(())
}

#[tokio::test]
async fn missing_embedder_falls_back_to_lexical() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    publish_search_fixture(store.clone(), td.path(), "openrouter", 2).await?;
    // auto_embed defaults to true and no embedder is configured: the recall
    // must degrade to lexical instead of failing.
    let rt = CairnRuntime::new(store, td.path().join("cache"));
    let response = rt
        .search("t", "kb", &search_request("rust", vec![]))
        .await?;
    assert_eq!(response.mode, SearchMode::Cold);
    assert_eq!(response.retrieval_mode, RetrievalMode::Lexical);
    assert!(response.query_embedding_fallback);
    assert!(
        response.hits.iter().any(|hit| hit.id == "rust-v1"),
        "lexical fallback must still return hits"
    );
    let value = serde_json::to_value(&response)?;
    assert_eq!(value["retrieval_mode"], "lexical");
    assert_eq!(value["query_embedding_fallback"], true);
    Ok(())
}

#[tokio::test]
async fn failing_embedder_falls_back_without_poisoning_cache() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    publish_search_fixture(store.clone(), td.path(), "openrouter", 2).await?;
    let fake = FakeEmbedder::failing();
    let rt =
        CairnRuntime::new(store, td.path().join("cache")).with_embedder(Arc::new(fake.clone()));

    let first = rt
        .search("t", "kb", &search_request("rust", vec![]))
        .await?;
    assert_eq!(first.retrieval_mode, RetrievalMode::Lexical);
    assert!(first.query_embedding_fallback);
    assert!(!first.hits.is_empty());

    let second = rt
        .search("t", "kb", &search_request("rust", vec![]))
        .await?;
    assert_eq!(second.retrieval_mode, RetrievalMode::Lexical);
    assert!(second.query_embedding_fallback);
    assert_eq!(
        fake.call_count(),
        2,
        "embedding errors must not be cached; the retry must call the embedder again"
    );

    fake.set_succeeding();
    let third = rt
        .search("t", "kb", &search_request("rust", vec![]))
        .await?;
    assert_eq!(third.retrieval_mode, RetrievalMode::Hybrid);
    assert!(!third.query_embedding_fallback);
    assert_eq!(fake.call_count(), 3);
    Ok(())
}

#[tokio::test]
async fn successful_embedder_yields_hybrid_and_caches_vector() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    publish_search_fixture(store.clone(), td.path(), "openrouter", 2).await?;
    let fake = FakeEmbedder::succeeding();
    let rt =
        CairnRuntime::new(store, td.path().join("cache")).with_embedder(Arc::new(fake.clone()));

    let response = rt
        .search("t", "kb", &search_request("rust", vec![]))
        .await?;
    assert_eq!(response.retrieval_mode, RetrievalMode::Hybrid);
    assert!(!response.query_embedding_fallback);
    let value = serde_json::to_value(&response)?;
    assert_eq!(value["retrieval_mode"], "hybrid");
    assert_eq!(value["query_embedding_fallback"], false);
    assert_eq!(fake.call_count(), 1);

    let cached = rt
        .search("t", "kb", &search_request("rust", vec![]))
        .await?;
    assert_eq!(cached.retrieval_mode, RetrievalMode::Hybrid);
    assert!(!cached.query_embedding_fallback);
    assert_eq!(
        fake.call_count(),
        1,
        "a successful query embedding must be cached"
    );
    Ok(())
}

#[tokio::test]
async fn explicit_vector_selects_hybrid_or_vector_and_never_falls_back() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    // External embeddings plus an explicit vector: auto-embedding is skipped,
    // so there is nothing to fall back from.
    publish_search_fixture(store.clone(), td.path(), "external", 2).await?;
    let rt = CairnRuntime::new(store, td.path().join("cache"));

    let hybrid = rt
        .search("t", "kb", &search_request("rust", vec![1.0, 0.0]))
        .await?;
    assert_eq!(hybrid.retrieval_mode, RetrievalMode::Hybrid);
    assert!(!hybrid.query_embedding_fallback);

    let vector_only = rt
        .search("t", "kb", &search_request("", vec![1.0, 0.0]))
        .await?;
    assert_eq!(vector_only.retrieval_mode, RetrievalMode::Vector);
    assert!(!vector_only.query_embedding_fallback);
    let value = serde_json::to_value(&vector_only)?;
    assert_eq!(value["retrieval_mode"], "vector");
    assert_eq!(value["query_embedding_fallback"], false);
    Ok(())
}

#[tokio::test]
async fn auto_embed_disabled_is_intentional_lexical_without_fallback() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    publish_search_fixture(store.clone(), td.path(), "openrouter", 2).await?;
    let fake = FakeEmbedder::succeeding();
    let mut rt =
        CairnRuntime::new(store, td.path().join("cache")).with_embedder(Arc::new(fake.clone()));
    rt.auto_embed = false;

    let response = rt
        .search("t", "kb", &search_request("rust", vec![]))
        .await?;
    assert_eq!(response.retrieval_mode, RetrievalMode::Lexical);
    assert!(
        !response.query_embedding_fallback,
        "an intentional lexical-only search is not a fallback"
    );
    assert_eq!(
        fake.call_count(),
        0,
        "auto_embed=false must not call the embedder"
    );
    Ok(())
}

#[tokio::test]
async fn second_search_and_lookup_reuse_opened_shard() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let counting = CountingStore::new(td.path().join("store"), None);
    let store: Arc<dyn ObjectStore> = Arc::new(counting.clone());
    publish_search_fixture(store.clone(), td.path(), "external", 2).await?;
    counting.arm();
    let mut rt = CairnRuntime::new(store, td.path().join("cache"));
    rt.auto_embed = false;

    let first = rt
        .search("t", "kb", &search_request("rust", vec![]))
        .await?;
    let (first_reads, first_bytes) = counting.shard_stats();
    assert!(first_reads > 0 && first_bytes > 0);

    let second = rt
        .search("t", "kb", &search_request("rust", vec![]))
        .await?;
    let (total_reads, total_bytes) = counting.shard_stats();
    let second_reads = total_reads - first_reads;
    let second_bytes = total_bytes - first_bytes;
    assert!(
        second_reads < first_reads && second_bytes < first_bytes,
        "cached opened shards must skip header/directory/meta reads: first={first_reads}r/{first_bytes}B second={second_reads}r/{second_bytes}B"
    );
    assert!(
        second_reads > 0 && second_bytes > 0,
        "payload/posting reads remain request-budgeted on cache hits"
    );
    assert!(
        second.remote_bytes < first.remote_bytes && second.range_reads < first.range_reads,
        "response accounting must reflect the opened-shard cache hit"
    );
    assert_eq!(
        serde_json::to_value(&first.hits)?,
        serde_json::to_value(&second.hits)?,
        "cache hits must not change results"
    );

    let lookup = lookup_request("rust");
    let first = rt.chunks_by_id_prefix("t", "kb", &lookup).await?;
    let (first_reads, first_bytes) = counting.shard_stats();
    let second = rt.chunks_by_id_prefix("t", "kb", &lookup).await?;
    let (total_reads, total_bytes) = counting.shard_stats();
    let second_reads = total_reads - first_reads;
    let second_bytes = total_bytes - first_bytes;
    assert!(
        second_reads < first_reads && second_bytes < first_bytes,
        "direct lookup must reuse the opened-shard cache"
    );
    assert!(
        second_reads > 0,
        "id/payload block reads remain budgeted on lookup cache hits"
    );
    assert_eq!(first.hits.len(), second.hits.len());
    assert_eq!(first.hits[0].id, second.hits[0].id);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_opens_singleflight_exactly_once() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    // Exact rendezvous: the first shard header read blocks until the test
    // thread arrives, guaranteeing all search tasks overlap the singleflight.
    let gate = Arc::new(Barrier::new(2));
    let counting = CountingStore::new(td.path().join("store"), Some(gate.clone()));
    let store: Arc<dyn ObjectStore> = Arc::new(counting.clone());
    publish_search_fixture(store.clone(), td.path(), "external", 1).await?;
    counting.arm();
    let mut rt = CairnRuntime::new(store, td.path().join("cache"));
    rt.auto_embed = false;
    let rt = Arc::new(rt);

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let rt = rt.clone();
        tasks.push(tokio::spawn(async move {
            rt.search("t", "kb", &search_request("rust", vec![])).await
        }));
    }
    gate.wait().await;
    let mut responses = Vec::new();
    for task in tasks {
        responses.push(task.await??);
    }
    assert_eq!(responses.len(), 8);
    for response in &responses {
        assert_eq!(response.retrieval_mode, RetrievalMode::Lexical);
        assert!(!response.hits.is_empty());
    }
    assert_eq!(
        counting.header_read_count(),
        1,
        "concurrent first opens must singleflight the shard open exactly once"
    );
    Ok(())
}
