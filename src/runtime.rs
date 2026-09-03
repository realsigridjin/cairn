use crate::cache::BundleCache;
use crate::embedding::{EmbeddingInputType, TextEmbedder};
use crate::heat::{HeatPolicy, HeatTracker};
use crate::manifest::{
    read_revision_stats, read_tombstones, Catalog, RevisionManifest, RevisionStatsDescriptor,
    ShardDescriptor,
};
use crate::model::{
    session_order_key, ChunkLookupHit, ChunkLookupOrder, ChunkLookupRequest, ChunkLookupResponse,
    RetrievalMode, RevisionStats, ScoreDomain, SearchMode, SearchRequest, SearchResponse,
};
use crate::object_store::ObjectStore;
use crate::search::cold::{ColdShardReader, OpenedColdShard, RemoteBudget};
use anyhow::{bail, Context, Result};
use dashmap::DashMap;
use futures::{stream, StreamExt, TryStreamExt};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

const MAX_STATS_CACHE_ENTRIES: usize = 1024;
const MAX_TOMBSTONE_CACHE_ENTRIES: usize = 1024;
const MAX_OPENED_SHARD_CACHE_ENTRIES: usize = 1024;
const MAX_QUERY_EMBEDDING_CACHE_ENTRIES: usize = 4096;
const MAX_QUERY_EMBEDDING_CACHE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct RuntimeLimits {
    pub max_remote_bytes_per_query: u64,
    pub max_range_reads_per_query: u64,
    pub max_candidate_limit: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_remote_bytes_per_query: 256 * 1024 * 1024,
            max_range_reads_per_query: 4096,
            max_candidate_limit: 20_000,
        }
    }
}

impl RuntimeLimits {
    pub fn validate(self) -> Result<Self> {
        if self.max_remote_bytes_per_query == 0 {
            bail!("runtime max remote bytes must be positive")
        }
        if self.max_range_reads_per_query == 0 {
            bail!("runtime max range reads must be positive")
        }
        if self.max_candidate_limit == 0 || self.max_candidate_limit > 100_000 {
            bail!("runtime max candidate limit must be in 1..=100000")
        }
        Ok(self)
    }
}

pub struct CairnRuntime {
    store: Arc<dyn ObjectStore>,
    pub catalog: Catalog,
    cache: BundleCache,
    heat: HeatTracker,
    stats_cache: DashMap<String, Arc<RevisionStats>>,
    stats_locks: DashMap<String, Arc<Mutex<()>>>,
    tombstone_cache: DashMap<String, Arc<HashSet<String>>>,
    tombstone_locks: DashMap<String, Arc<Mutex<()>>>,
    query_embedding_cache: DashMap<String, Arc<Vec<f32>>>,
    query_embedding_locks: DashMap<String, Arc<Mutex<()>>>,
    opened_shard_cache: DashMap<String, Arc<OpenedColdShard>>,
    opened_shard_locks: DashMap<String, Arc<Mutex<()>>>,
    activation_inflight: Arc<DashMap<String, ()>>,
    pub nprobe: usize,
    pub limits: RuntimeLimits,
    pub auto_embed: bool,
    embedder: Option<Arc<dyn TextEmbedder>>,
}

impl CairnRuntime {
    pub fn new(store: Arc<dyn ObjectStore>, cache_root: impl Into<std::path::PathBuf>) -> Self {
        Self::new_with_cache_limit(store, cache_root, None)
    }

    pub fn new_with_cache_limit(
        store: Arc<dyn ObjectStore>,
        cache_root: impl Into<std::path::PathBuf>,
        max_cache_bytes: Option<u64>,
    ) -> Self {
        Self {
            catalog: Catalog::new(store.clone()),
            cache: BundleCache::new_with_limit(cache_root, store.clone(), max_cache_bytes),
            heat: HeatTracker::new(HeatPolicy::default()),
            stats_cache: DashMap::new(),
            stats_locks: DashMap::new(),
            tombstone_cache: DashMap::new(),
            tombstone_locks: DashMap::new(),
            query_embedding_cache: DashMap::new(),
            query_embedding_locks: DashMap::new(),
            opened_shard_cache: DashMap::new(),
            opened_shard_locks: DashMap::new(),
            activation_inflight: Arc::new(DashMap::new()),
            store,
            nprobe: 8,
            limits: RuntimeLimits::default(),
            auto_embed: true,
            embedder: None,
        }
    }

    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn TextEmbedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    pub async fn search(
        &self,
        tenant: &str,
        kb: &str,
        req: &SearchRequest,
    ) -> Result<SearchResponse> {
        req.validate()?;
        let limits = self.limits.validate()?;
        if req.max_remote_bytes > limits.max_remote_bytes_per_query {
            bail!("request remote-byte budget exceeds runtime policy")
        }
        if req.max_range_reads > limits.max_range_reads_per_query {
            bail!("request range-read budget exceeds runtime policy")
        }
        if req.candidate_limit > limits.max_candidate_limit {
            bail!("request candidate_limit exceeds runtime policy")
        }
        if self.nprobe == 0 || self.nprobe > 256 {
            bail!("runtime nprobe must be in 1..=256")
        }

        let manifest = self.catalog.resolve(tenant, kb, req.revision).await?;
        // Validate and load revision-global statistics before a potentially paid
        // remote embedding call. A corrupt/incomplete revision should fail
        // before spending embedding credits.
        let budget = Arc::new(RemoteBudget::new(req.max_remote_bytes, req.max_range_reads));
        let (stats, stats_bytes, stats_reads) =
            self.load_stats(&manifest.stats, budget.clone()).await?;

        let mut effective_request = req.clone();
        let mut query_embedding_fallback = false;
        if self.auto_embed
            && effective_request.query_vector.is_empty()
            && !effective_request.query.trim().is_empty()
        {
            match manifest.embedding_provider.as_str() {
                "openrouter" => match self.embedder.as_ref() {
                    Some(embedder) => {
                        match self
                            .load_query_embedding(embedder, &manifest, &effective_request.query)
                            .await
                        {
                            Ok(vector) => {
                                effective_request.query_vector = vector;
                            }
                            Err(error) => {
                                tracing::warn!(
                                    error = %error,
                                    "automatic query embedding failed; falling back to lexical retrieval"
                                );
                                query_embedding_fallback = true;
                            }
                        }
                    }
                    None => {
                        tracing::warn!(
                            model = %manifest.embedding_model,
                            "no embedder is configured for this revision; falling back to lexical retrieval"
                        );
                        query_embedding_fallback = true;
                    }
                },
                "external" => {
                    tracing::warn!(
                        model = %manifest.embedding_model,
                        "revision uses external embeddings; falling back to lexical retrieval"
                    );
                    query_embedding_fallback = true;
                }
                other => bail!("unsupported revision embedding provider: {other}"),
            }
        }
        let req = &effective_request;
        if !req.query_vector.is_empty() && req.query_vector.len() != manifest.dimension as usize {
            bail!(
                "query vector dimension mismatch: got {}, expected {}",
                req.query_vector.len(),
                manifest.dimension
            )
        }
        let retrieval_mode = req.retrieval_mode();

        let heat_key = format!("{}:{}:{}", tenant, kb, manifest.revision);
        if crate::uqa::warm_execution_enabled() && self.heat.should_promote(&heat_key) {
            if let Some(bundle) = &manifest.uqa_bundle {
                if bundle.materialized_revision == manifest.revision {
                    match self.cache.lease_if_ready(bundle).await {
                        Ok(Some(lease)) => match crate::uqa::search_uqa(
                            lease.path(),
                            req,
                            &stats,
                            manifest.revision,
                            manifest.dimension,
                        )
                        .await
                        {
                            Ok(mut hits) => {
                                // Shape the wire payload only after ranking,
                                // filtering and verification have run on full
                                // text, identically to the cold path below.
                                for hit in &mut hits {
                                    hit.apply_text_budget(req.max_text_bytes);
                                }
                                return Ok(SearchResponse {
                                    revision: manifest.revision,
                                    embedding_provider: manifest.embedding_provider.clone(),
                                    embedding_model: manifest.embedding_model.clone(),
                                    dimension: manifest.dimension,
                                    corpus_sha256: stats.corpus_sha256.clone(),
                                    mode: SearchMode::Warm,
                                    retrieval_mode,
                                    query_embedding_fallback,
                                    score_domain: ScoreDomain::RevisionCalibratedLogOdds,
                                    approximate: !req.query_vector.is_empty()
                                        || !req.filters.is_empty()
                                        || !req.require_text.is_empty(),
                                    hits,
                                    remote_bytes: stats_bytes
                                        .saturating_add(lease.downloaded_bytes()),
                                    range_reads: stats_reads
                                        .saturating_add(u64::from(lease.downloaded_bytes() > 0)),
                                });
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "warm UQA search failed; falling back to cold shard search")
                            }
                        },
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "warm UQA cache validation failed; falling back to cold shard search")
                        }
                    }
                }
            }
        }

        let mut response = self
            .search_cold(
                &manifest,
                req,
                &stats,
                budget.clone(),
                retrieval_mode,
                query_embedding_fallback,
            )
            .await?;
        response.remote_bytes = response.remote_bytes.saturating_add(stats_bytes);
        response.range_reads = response.range_reads.saturating_add(stats_reads);
        for hit in &mut response.hits {
            hit.apply_text_budget(req.max_text_bytes);
        }

        if crate::uqa::warm_execution_enabled() {
            if let Some(bundle) = &manifest.uqa_bundle {
                if self
                    .heat
                    .record_query(&heat_key, response.remote_bytes, bundle.size)
                    && self
                        .activation_inflight
                        .insert(bundle.sha256.clone(), ())
                        .is_none()
                {
                    let cache = self.cache.clone();
                    let bundle = bundle.clone();
                    let inflight = self.activation_inflight.clone();
                    tokio::spawn(async move {
                        if let Err(error) = cache.activate(&bundle).await {
                            tracing::warn!(%error, digest = %bundle.sha256, "background UQA activation failed");
                        }
                        inflight.remove(&bundle.sha256);
                    });
                }
            }
        }
        Ok(response)
    }

    /// Direct chunk lookup by immutable id prefix. This is a point-read path:
    /// it resolves the pinned revision manifest, honors tombstones and the
    /// runtime/request remote budgets, scans only the compact `ids` blocks of
    /// each shard, and reads payload blocks solely for matching documents.
    /// Results are globally sorted by id and truncated to `limit`. It never
    /// touches the lexical/vector indexes, the warm UQA path, or normal search.
    pub async fn chunks_by_id_prefix(
        &self,
        tenant: &str,
        kb: &str,
        req: &ChunkLookupRequest,
    ) -> Result<ChunkLookupResponse> {
        req.validate()?;
        let limits = self.limits.validate()?;
        if req.max_remote_bytes > limits.max_remote_bytes_per_query {
            bail!("request remote-byte budget exceeds runtime policy")
        }
        if req.max_range_reads > limits.max_range_reads_per_query {
            bail!("request range-read budget exceeds runtime policy")
        }

        let manifest = self.catalog.resolve(tenant, kb, req.revision).await?;
        let budget = Arc::new(RemoteBudget::new(req.max_remote_bytes, req.max_range_reads));
        // Load revision-global statistics to pin the live-corpus digest into
        // the response provenance. Charged to the same request budget.
        let (stats, stats_bytes, stats_reads) =
            self.load_stats(&manifest.stats, budget.clone()).await?;
        let (tombstones, tombstone_bytes, tombstone_reads) =
            self.load_tombstones(&manifest, budget.clone()).await?;

        let budget_for_scan = budget.clone();
        let mut scanned: Vec<(usize, Vec<ChunkLookupHit>, u64, u64)> =
            stream::iter(manifest.shards.iter().cloned().enumerate())
                .map(|(ordinal, shard)| {
                    let budget = budget_for_scan.clone();
                    let prefix = req.id_prefix.clone();
                    let tombstones = tombstones.clone();
                    async move {
                        let (reader, opened) = self.open_shard(shard, budget).await?;
                        let hits = reader
                            .chunks_by_id_prefix_opened(&opened, &prefix, &tombstones)
                            .await?;
                        let after = reader.stats.snapshot();
                        Ok::<_, anyhow::Error>((ordinal, hits, after.0, after.1))
                    }
                })
                .buffer_unordered(16)
                .try_collect()
                .await?;
        scanned.sort_by_key(|entry| entry.0);

        let mut remote_bytes = tombstone_bytes.saturating_add(stats_bytes);
        let mut range_reads = tombstone_reads.saturating_add(stats_reads);
        // Merge in shard-ordinal order. Chunk IDs are immutable/versioned
        // identities; if malformed input places the same live ID in more than
        // one shard, the lowest-ordinal shard wins deterministically.
        let mut by_id: std::collections::BTreeMap<String, ChunkLookupHit> =
            std::collections::BTreeMap::new();
        for (_, hits, bytes, reads) in scanned {
            remote_bytes = remote_bytes.saturating_add(bytes);
            range_reads = range_reads.saturating_add(reads);
            for hit in hits {
                by_id.entry(hit.id.clone()).or_insert(hit);
            }
        }
        let mut hits: Vec<_> = by_id.into_values().collect();
        if req.order_by == ChunkLookupOrder::SeqStart {
            // Order matched payloads chronologically BEFORE the global limit
            // so bounded requests keep the earliest valid session windows.
            hits.sort_by(|left, right| {
                session_order_key(left)
                    .cmp(&session_order_key(right))
                    .then_with(|| left.id.cmp(&right.id))
            });
        }
        hits.truncate(req.limit);
        Ok(ChunkLookupResponse {
            revision: manifest.revision,
            embedding_provider: manifest.embedding_provider.clone(),
            embedding_model: manifest.embedding_model.clone(),
            dimension: manifest.dimension,
            corpus_sha256: stats.corpus_sha256.clone(),
            hits,
            remote_bytes,
            range_reads,
        })
    }

    /// Embed the query text through the configured provider with a bounded
    /// singleflight cache. Only validated, correctly-dimensioned vectors are
    /// cached; provider errors and empty/wrong-dimension results propagate so
    /// the caller can degrade to lexical retrieval.
    async fn load_query_embedding(
        &self,
        embedder: &Arc<dyn TextEmbedder>,
        manifest: &RevisionManifest,
        query: &str,
    ) -> Result<Vec<f32>> {
        let cache_key =
            query_embedding_cache_key(&manifest.embedding_model, manifest.dimension, query);
        if let Some(cached) = self.query_embedding_cache.get(&cache_key) {
            return Ok(cached.value().as_ref().clone());
        }
        let lock = self
            .query_embedding_locks
            .entry(cache_key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let guard = lock.lock().await;
        let loaded: Result<Vec<f32>> = async {
            if let Some(cached) = self.query_embedding_cache.get(&cache_key) {
                return Ok(cached.value().as_ref().clone());
            }
            let input = [query];
            let mut embedded = embedder
                .embed(
                    &manifest.embedding_model,
                    manifest.dimension,
                    EmbeddingInputType::Query,
                    &input,
                )
                .await?;
            let vector = embedded
                .pop()
                .context("embedding provider returned no query vector")?;
            anyhow::ensure!(
                vector.len() == manifest.dimension as usize,
                "embedding provider returned dimension {}, expected {}",
                vector.len(),
                manifest.dimension
            );
            self.query_embedding_cache
                .insert(cache_key.clone(), Arc::new(vector.clone()));
            let vector_bytes = vector
                .len()
                .saturating_mul(std::mem::size_of::<f32>())
                .max(1);
            let cache_entries = (MAX_QUERY_EMBEDDING_CACHE_BYTES / vector_bytes)
                .clamp(1, MAX_QUERY_EMBEDDING_CACHE_ENTRIES);
            trim_dashmap(&self.query_embedding_cache, cache_entries, Some(&cache_key));
            Ok(vector)
        }
        .await;
        drop(guard);
        cleanup_mutex_entry(&self.query_embedding_locks, &cache_key, &lock);
        loaded
    }

    /// Open a cold shard through the bounded immutable opened-shard cache,
    /// keyed by the content-addressed shard identity. Cache hits consume zero
    /// request budget for header/directory/meta reads; misses singleflight
    /// per key so concurrent first opens read exactly once. Open errors are
    /// never cached.
    async fn open_shard(
        &self,
        shard: ShardDescriptor,
        budget: Arc<RemoteBudget>,
    ) -> Result<(ColdShardReader, Arc<OpenedColdShard>)> {
        let reader = ColdShardReader::new_with_budget(self.store.clone(), shard.clone(), budget);
        let key = opened_shard_cache_key(&shard);
        if let Some(cached) = self.opened_shard_cache.get(&key) {
            let opened = cached.value().clone();
            drop(cached);
            return Ok((reader, opened));
        }
        let lock = self
            .opened_shard_locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let guard = lock.lock().await;
        if let Some(cached) = self.opened_shard_cache.get(&key) {
            let opened = cached.value().clone();
            drop(cached);
            drop(guard);
            cleanup_mutex_entry(&self.opened_shard_locks, &key, &lock);
            return Ok((reader, opened));
        }
        let loaded = reader.open().await;
        let opened = match loaded {
            Ok(opened) => {
                // Insert while still holding the singleflight guard so queued
                // waiters observe the opened shard instead of opening again.
                let opened = Arc::new(opened);
                self.opened_shard_cache.insert(key.clone(), opened.clone());
                trim_dashmap(
                    &self.opened_shard_cache,
                    MAX_OPENED_SHARD_CACHE_ENTRIES,
                    Some(&key),
                );
                opened
            }
            Err(error) => {
                drop(guard);
                cleanup_mutex_entry(&self.opened_shard_locks, &key, &lock);
                return Err(error);
            }
        };
        drop(guard);
        cleanup_mutex_entry(&self.opened_shard_locks, &key, &lock);
        Ok((reader, opened))
    }

    async fn load_stats(
        &self,
        d: &RevisionStatsDescriptor,
        budget: Arc<RemoteBudget>,
    ) -> Result<(Arc<RevisionStats>, u64, u64)> {
        if let Some(v) = self.stats_cache.get(&d.sha256) {
            return Ok((v.clone(), 0, 0));
        }
        let lock = self
            .stats_locks
            .entry(d.sha256.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let guard = lock.lock().await;
        if let Some(v) = self.stats_cache.get(&d.sha256) {
            let cached = v.clone();
            drop(v);
            drop(guard);
            cleanup_mutex_entry(&self.stats_locks, &d.sha256, &lock);
            return Ok((cached, 0, 0));
        }
        // Reserve before issuing the remote read. Charging only after the read
        // makes the budget an accounting metric rather than an actual guard.
        // Keep cleanup outside the fallible operation so a budget/read failure
        // cannot strand this singleflight lock entry forever.
        let loaded = match budget.charge(d.size, 1) {
            Ok(()) => read_revision_stats(self.store.clone(), d).await,
            Err(error) => Err(error),
        };
        drop(guard);
        cleanup_mutex_entry(&self.stats_locks, &d.sha256, &lock);
        let (stats, bytes, reads) = loaded?;
        let stats = Arc::new(stats);
        self.stats_cache.insert(d.sha256.clone(), stats.clone());
        trim_dashmap(&self.stats_cache, MAX_STATS_CACHE_ENTRIES, Some(&d.sha256));
        Ok((stats, bytes, reads))
    }

    async fn load_tombstones(
        &self,
        manifest: &RevisionManifest,
        budget: Arc<RemoteBudget>,
    ) -> Result<(Arc<HashSet<String>>, u64, u64)> {
        if manifest.tombstones.is_empty() {
            return Ok((Arc::new(HashSet::new()), 0, 0));
        }
        let key = tombstone_cache_key(manifest);
        if let Some(v) = self.tombstone_cache.get(&key) {
            return Ok((v.clone(), 0, 0));
        }
        let lock = self
            .tombstone_locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let guard = lock.lock().await;
        if let Some(v) = self.tombstone_cache.get(&key) {
            let cached = v.clone();
            drop(v);
            drop(guard);
            cleanup_mutex_entry(&self.tombstone_locks, &key, &lock);
            return Ok((cached, 0, 0));
        }
        let planned = manifest
            .tombstones
            .iter()
            .try_fold(0u64, |sum, d| {
                sum.checked_add(d.size)
                    .ok_or_else(|| anyhow::anyhow!("tombstone byte budget overflow"))
            })
            .and_then(|bytes| {
                u64::try_from(manifest.tombstones.len())
                    .map(|reads| (bytes, reads))
                    .map_err(|_| anyhow::anyhow!("too many tombstone objects"))
            });
        let loaded = match planned.and_then(|(bytes, reads)| budget.charge(bytes, reads)) {
            Ok(()) => read_tombstones(self.store.clone(), &manifest.tombstones).await,
            Err(error) => Err(error),
        };
        drop(guard);
        cleanup_mutex_entry(&self.tombstone_locks, &key, &lock);
        let (set, bytes, reads) = loaded?;
        let set = Arc::new(set);
        self.tombstone_cache.insert(key.clone(), set.clone());
        trim_dashmap(
            &self.tombstone_cache,
            MAX_TOMBSTONE_CACHE_ENTRIES,
            Some(&key),
        );
        Ok((set, bytes, reads))
    }

    async fn search_cold(
        &self,
        manifest: &RevisionManifest,
        req: &SearchRequest,
        stats: &RevisionStats,
        budget: Arc<RemoteBudget>,
        retrieval_mode: RetrievalMode,
        query_embedding_fallback: bool,
    ) -> Result<SearchResponse> {
        struct CandidateShard {
            ordinal: usize,
            upper_bound: f32,
            reader: ColdShardReader,
            opened: Arc<OpenedColdShard>,
        }

        let (tombstones, tombstone_bytes, tombstone_reads) =
            self.load_tombstones(manifest, budget.clone()).await?;
        let manifest_dimension = manifest.dimension;
        let budget_for_open = budget.clone();
        let mut readers: Vec<(CandidateShard, u64, u64)> =
            stream::iter(manifest.shards.iter().cloned().enumerate())
                .map(|(ordinal, shard)| {
                    let budget = budget_for_open.clone();
                    async move {
                        let (reader, opened) = self.open_shard(shard, budget).await?;
                        if opened.meta.dimension != manifest_dimension {
                            bail!("shard dimension does not match manifest")
                        }
                        if opened.meta.analyzer != stats.analyzer {
                            bail!(
                                "shard analyzer {} does not match revision stats analyzer {}",
                                opened.meta.analyzer,
                                stats.analyzer
                            )
                        }
                        let upper_bound = reader.score_upper_bound_opened(&opened, req, stats)?;
                        let after = reader.stats.snapshot();
                        Ok::<_, anyhow::Error>((
                            CandidateShard {
                                ordinal,
                                upper_bound,
                                reader,
                                opened,
                            },
                            after.0,
                            after.1,
                        ))
                    }
                })
                .buffer_unordered(16)
                .try_collect()
                .await?;
        let mut remote_bytes = tombstone_bytes;
        let mut range_reads = tombstone_reads;
        for (_, bytes, reads) in &readers {
            remote_bytes = remote_bytes.saturating_add(*bytes);
            range_reads = range_reads.saturating_add(*reads);
        }
        let mut readers: Vec<CandidateShard> = readers.drain(..).map(|x| x.0).collect();
        readers.sort_by(|a, b| {
            b.upper_bound
                .total_cmp(&a.upper_bound)
                .then_with(|| b.ordinal.cmp(&a.ordinal))
        });

        // Chunk IDs are immutable/versioned identities. Updates MUST tombstone the old
        // ID and insert a new ID. If malformed input places the same live ID in more
        // than one shard, retain the highest-scoring candidate. This keeps score-bound
        // pruning sound; "latest shard wins" would be unsound because a newer duplicate
        // could itself be pruned before it shadows an older hit.
        let mut best_by_id: HashMap<String, crate::model::SearchHit> = HashMap::new();
        let mut threshold = f32::NEG_INFINITY;
        for entry in readers {
            if best_by_id.len() >= req.limit && entry.upper_bound < threshold {
                continue;
            }
            let before = entry.reader.stats.snapshot();
            let hits = entry
                .reader
                .search_opened(&entry.opened, req, self.nprobe, &tombstones, stats)
                .await?;
            let after = entry.reader.stats.snapshot();
            remote_bytes = remote_bytes.saturating_add(after.0.saturating_sub(before.0));
            range_reads = range_reads.saturating_add(after.1.saturating_sub(before.1));
            for hit in hits {
                match best_by_id.get(&hit.id) {
                    Some(previous) if previous.score >= hit.score => {}
                    _ => {
                        best_by_id.insert(hit.id.clone(), hit);
                    }
                }
            }
            let mut current: Vec<_> = best_by_id.values().collect();
            current.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
            if current.len() >= req.limit {
                threshold = current[req.limit - 1].score;
            }
        }

        let mut hits: Vec<_> = best_by_id.into_values().collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(req.limit);
        Ok(SearchResponse {
            revision: manifest.revision,
            embedding_provider: manifest.embedding_provider.clone(),
            embedding_model: manifest.embedding_model.clone(),
            dimension: manifest.dimension,
            corpus_sha256: stats.corpus_sha256.clone(),
            mode: SearchMode::Cold,
            retrieval_mode,
            query_embedding_fallback,
            score_domain: ScoreDomain::RevisionCalibratedLogOdds,
            // Metadata filtering and exact verification both run after a
            // bounded candidate pool, so neither can promise an exact top-k.
            approximate: !req.query_vector.is_empty()
                || !req.filters.is_empty()
                || !req.require_text.is_empty(),
            hits,
            remote_bytes,
            range_reads,
        })
    }
}

fn query_embedding_cache_key(model: &str, dimension: u32, query: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(model.as_bytes());
    digest.update([0]);
    digest.update(dimension.to_le_bytes());
    digest.update([0]);
    digest.update(query.as_bytes());
    format!("{:x}", digest.finalize())
}

/// Content-addressed identity of an immutable shard object. The declared
/// format version and directory digest are included so a manifest that pairs
/// known content with different structural claims cannot bypass open-time
/// validation through a cache hit.
fn opened_shard_cache_key(d: &ShardDescriptor) -> String {
    format!("{}:{}:{}", d.format_version, d.directory_sha256, d.sha256)
}

fn tombstone_cache_key(m: &RevisionManifest) -> String {
    let mut key = format!("{}:{}:{}:", m.tenant, m.knowledge_base, m.revision);
    for d in &m.tombstones {
        key.push_str(&d.sha256);
        key.push(':');
    }
    key
}

fn trim_dashmap<V>(map: &DashMap<String, V>, max: usize, keep: Option<&str>) {
    if map.len() <= max {
        return;
    }
    let keys: Vec<String> = map.iter().map(|x| x.key().clone()).collect();
    for key in keys {
        if map.len() <= max {
            break;
        }
        if keep == Some(key.as_str()) {
            continue;
        }
        map.remove(&key);
    }
}

fn cleanup_mutex_entry(map: &DashMap<String, Arc<Mutex<()>>>, key: &str, lock: &Arc<Mutex<()>>) {
    // Remove only when the map and this function are the last Arc owners. A
    // waiter clones the Arc before awaiting, so strong_count > 2 prevents us
    // from deleting the entry and accidentally creating a second lock for the
    // same key while that waiter is still queued.
    let _ = map.remove_if(key, |_, current| {
        Arc::ptr_eq(current, lock) && Arc::strong_count(current) == 2
    });
}
