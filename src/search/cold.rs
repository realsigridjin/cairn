use crate::binary::{
    decode_block, decode_block_raw, decode_directory, Directory, Header, HEADER_SIZE,
    MAX_BLOCK_RAW, MAX_COMPRESSED_BLOCK_BYTES, MAX_DIRECTORY_BYTES,
};
use crate::index::lexical::{bm25, query_terms, PostingList};
use crate::index::vector::{approx_dot, normalize, top_centroids, IvfList, Router};
use crate::index::{IdRecord, PayloadRecord, ShardMeta};
use crate::manifest::ShardDescriptor;
use crate::model::{ChunkLookupHit, RevisionStats, SearchHit, SearchRequest};
use crate::object_store::ObjectStore;
use crate::search::fusion::merge_evidence;
use crate::search::verify::TextVerifier;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures::{stream, StreamExt, TryStreamExt};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

const MAX_QUERY_TERMS: usize = 256;
const MAX_CONCURRENT_RANGE_READS: usize = 16;
const MAX_LEXICAL_ACCUMULATOR_DOCS: usize = 2_000_000;

async fn decode_directory_async(header: Header, bytes: Bytes) -> Result<Directory> {
    tokio::task::spawn_blocking(move || decode_directory(header, bytes.as_ref()))
        .await
        .context("join directory decode task")?
}

async fn decode_block_async<T>(block: crate::binary::BlockRef, bytes: Bytes) -> Result<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    tokio::task::spawn_blocking(move || decode_block(&block, bytes.as_ref(), MAX_BLOCK_RAW))
        .await
        .context("join block decode task")?
}

async fn decode_block_raw_async(block: crate::binary::BlockRef, bytes: Bytes) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || decode_block_raw(&block, bytes.as_ref(), MAX_BLOCK_RAW))
        .await
        .context("join raw block decode task")?
}

#[derive(Debug, Default)]
pub struct IoStats {
    pub bytes: AtomicU64,
    pub reads: AtomicU64,
}
impl IoStats {
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.bytes.load(Ordering::Relaxed),
            self.reads.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug)]
pub struct RemoteBudget {
    max_bytes: u64,
    max_reads: u64,
    bytes: AtomicU64,
    reads: AtomicU64,
}

impl RemoteBudget {
    pub fn new(max_bytes: u64, max_reads: u64) -> Self {
        Self {
            max_bytes,
            max_reads,
            bytes: AtomicU64::new(0),
            reads: AtomicU64::new(0),
        }
    }

    pub fn charge(&self, bytes: u64, reads: u64) -> Result<()> {
        reserve(&self.bytes, bytes, self.max_bytes, "remote byte")?;
        if let Err(error) = reserve(&self.reads, reads, self.max_reads, "range-read") {
            self.bytes.fetch_sub(bytes, Ordering::AcqRel);
            return Err(error);
        }
        Ok(())
    }
}

fn reserve(counter: &AtomicU64, amount: u64, limit: u64, label: &str) -> Result<()> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let next = current
            .checked_add(amount)
            .context("remote budget overflow")?;
        if next > limit {
            bail!("{label} budget exceeded: {next} > {limit}")
        }
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(()),
            Err(actual) => current = actual,
        }
    }
}

#[derive(Clone)]
pub struct OpenedColdShard {
    pub directory: Directory,
    pub meta: ShardMeta,
}

#[derive(Clone)]
pub struct ColdShardReader {
    store: Arc<dyn ObjectStore>,
    shard: ShardDescriptor,
    budget: Arc<RemoteBudget>,
    pub stats: Arc<IoStats>,
}

impl ColdShardReader {
    pub fn new(store: Arc<dyn ObjectStore>, shard: ShardDescriptor) -> Self {
        Self::new_with_budget(
            store,
            shard,
            Arc::new(RemoteBudget::new(u64::MAX, u64::MAX)),
        )
    }

    pub fn new_with_budget(
        store: Arc<dyn ObjectStore>,
        shard: ShardDescriptor,
        budget: Arc<RemoteBudget>,
    ) -> Self {
        Self {
            store,
            shard,
            budget,
            stats: Arc::new(IoStats::default()),
        }
    }

    async fn range(&self, offset: u64, len: u64) -> Result<Bytes> {
        let end = offset.checked_add(len).context("range overflow")?;
        if end > self.shard.size {
            bail!("range exceeds declared shard size")
        }
        self.budget.charge(len, 1)?;
        let b = self.store.get_range(&self.shard.key, offset, len).await?;
        self.stats.reads.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes
            .fetch_add(b.len() as u64, Ordering::Relaxed);
        Ok(b)
    }

    pub async fn open_directory(&self) -> Result<(Header, Directory)> {
        if self.shard.format_version != crate::binary::FORMAT_VERSION {
            bail!("unsupported shard descriptor format")
        }
        let hbytes = self.range(0, HEADER_SIZE as u64).await?;
        let header = Header::decode(&hbytes)?;
        if header.directory_sha256_hex() != self.shard.directory_sha256 {
            bail!("manifest/header directory digest mismatch")
        }
        if header.dir_len == 0 || header.dir_len > MAX_DIRECTORY_BYTES {
            bail!("invalid directory size {}", header.dir_len)
        }
        let dir_end = header
            .dir_offset
            .checked_add(header.dir_len)
            .context("directory range overflow")?;
        if dir_end != self.shard.size {
            bail!("directory does not terminate declared shard")
        }
        let db = self.range(header.dir_offset, header.dir_len).await?;
        let dir = decode_directory_async(header, db).await?;
        dir.validate_layout(self.shard.size, header.dir_offset, header.dir_len)?;
        Ok((header, dir))
    }

    async fn block<T>(&self, dir: &Directory, kind: &str, key: &str) -> Result<T>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        let br = dir.get(kind, key)?.clone();
        if br.len > MAX_COMPRESSED_BLOCK_BYTES {
            bail!("compressed block too large: {}", br.len)
        }
        let bytes = self.range(br.offset, br.len).await?;
        decode_block_async(br, bytes).await
    }

    pub async fn open(&self) -> Result<OpenedColdShard> {
        let (_, directory) = self.open_directory().await?;
        let meta: ShardMeta = self.block(&directory, "meta", "main").await?;
        meta.validate()?;
        crate::index::validate_directory_schema(&directory, &meta)?;
        Ok(OpenedColdShard { directory, meta })
    }

    pub fn score_upper_bound_opened(
        &self,
        opened: &OpenedColdShard,
        req: &SearchRequest,
        stats: &RevisionStats,
    ) -> Result<f32> {
        let meta = &opened.meta;
        let terms = checked_query_terms(&req.query)?;
        let mut lexical_raw = 0.0f32;
        for term in terms {
            let Some(bound) = meta.term_bounds.get(&term) else {
                continue;
            };
            let Some(df) = stats.term_df.get(&term) else {
                continue;
            };
            lexical_raw += bm25(
                bound.max_tf,
                bound.min_doc_len,
                *df,
                stats.live_document_count as u32,
                stats.average_document_length,
                stats.scoring.bm25_k1,
                stats.scoring.bm25_b,
            );
        }
        let lexical = if lexical_raw > 0.0 {
            let lo = stats
                .scoring
                .cold
                .lexical
                .evidence_llr(0.0, stats.scoring.base_rate);
            let hi = stats
                .scoring
                .cold
                .lexical
                .evidence_llr(lexical_raw, stats.scoring.base_rate);
            modality_upper_bound(lo, hi, stats.scoring.lexical_weight)
        } else {
            0.0
        };
        let vector = if req.query_vector.is_empty() {
            0.0
        } else {
            let lo = stats
                .scoring
                .cold
                .vector
                .evidence_llr(-1.0, stats.scoring.base_rate);
            let hi = stats
                .scoring
                .cold
                .vector
                .evidence_llr(1.0, stats.scoring.base_rate);
            modality_upper_bound(lo, hi, stats.scoring.vector_weight)
        };
        Ok(crate::model::logit(stats.scoring.base_rate) + lexical + vector)
    }

    pub async fn search_opened(
        &self,
        opened: &OpenedColdShard,
        req: &SearchRequest,
        nprobe: usize,
        excluded_ids: &HashSet<String>,
        stats: &RevisionStats,
    ) -> Result<Vec<SearchHit>> {
        let dir = &opened.directory;
        let meta = &opened.meta;
        if !req.query_vector.is_empty() && req.query_vector.len() != meta.dimension as usize {
            bail!(
                "query vector dimension {}, expected {}",
                req.query_vector.len(),
                meta.dimension
            )
        }
        let terms = checked_query_terms(&req.query)?;
        let lexical = self.lexical(dir, meta, &terms, stats).await?;
        let vector = if !req.query_vector.is_empty() {
            let approx = self
                .vector(dir, meta, &req.query_vector, nprobe, req.candidate_limit)
                .await?;
            self.exact_vector_evidence(
                dir,
                meta,
                &req.query_vector,
                approx.into_iter().map(|x| x.0).collect(),
                stats,
            )
            .await?
        } else {
            Vec::new()
        };
        let evidence = merge_evidence(lexical, vector);
        let mut ranked: Vec<_> = evidence
            .into_iter()
            .map(|(doc_idx, e)| {
                let logit = e.fused_logit(&stats.scoring);
                let post = e.posterior(&stats.scoring);
                (doc_idx, e, logit, post)
            })
            .collect();
        ranked.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

        // Tombstones are correctness semantics, not a best-effort metadata filter.
        // Filtering them only after `candidate_limit` truncation allows deleted
        // high-scoring documents to crowd live documents out of the candidate
        // pool. A compact doc_idx -> immutable chunk-id sidecar lets us remove
        // tombstoned candidates before applying the pool bound without fetching
        // full payload text/metadata.
        let ranked = self
            .retain_live_candidates(
                dir,
                meta,
                ranked,
                excluded_ids,
                req.effective_candidate_limit(),
            )
            .await?;
        let payloads = self.payloads(dir, meta, ranked.iter().map(|x| x.0)).await?;
        // Verification runs on the full payload text, before `limit`
        // truncation, so narrowing costs recall no matching document.
        let verifier = TextVerifier::new(req);
        let mut hits = Vec::with_capacity(req.limit.max(1));
        for (doc_idx, e, score, posterior) in ranked {
            if let Some(p) = payloads.get(&doc_idx) {
                if metadata_matches(&p.metadata, &req.filters) && verifier.verify(&p.text) {
                    hits.push(SearchHit {
                        id: p.id.clone(),
                        score,
                        posterior,
                        lexical_evidence: e.lexical,
                        vector_evidence: e.vector,
                        text: p.text.clone(),
                        text_truncated: false,
                        metadata: p.metadata.clone(),
                    });
                    if hits.len() >= req.limit.max(1) {
                        break;
                    }
                }
            }
        }
        Ok(hits)
    }

    /// Scan the compact `ids` blocks of an already-opened shard for chunk ids
    /// starting with `id_prefix`, then load only the payload blocks that hold
    /// matching documents. Tombstoned ids are excluded before any payload read.
    ///
    /// The opened shard is passed in (rather than re-opened here) so callers
    /// that hold a directory/open-shard cache can reuse the same
    /// [`OpenedColdShard`] value across requests.
    pub async fn chunks_by_id_prefix_opened(
        &self,
        opened: &OpenedColdShard,
        id_prefix: &str,
        excluded_ids: &HashSet<String>,
    ) -> Result<Vec<ChunkLookupHit>> {
        let dir = &opened.directory;
        let meta = &opened.meta;
        let id_block_count = meta.document_count.div_ceil(meta.id_block_size);
        let mut jobs = Vec::new();
        for block_id in 0..id_block_count {
            let me = self.clone();
            let dir = dir.clone();
            jobs.push(async move {
                me.block::<Vec<IdRecord>>(&dir, "ids", &block_id.to_string())
                    .await
            });
        }
        let blocks = stream::iter(jobs)
            .buffer_unordered(MAX_CONCURRENT_RANGE_READS)
            .try_collect::<Vec<_>>()
            .await?;
        let mut matched: BTreeMap<u32, String> = BTreeMap::new();
        for block in blocks {
            for record in block {
                validate_id_record(meta, &record)?;
                if record.id.starts_with(id_prefix)
                    && !excluded_ids.contains(&record.id)
                    && matched.insert(record.doc_idx, record.id).is_some()
                {
                    bail!("duplicate id record doc_idx")
                }
            }
        }
        if matched.is_empty() {
            return Ok(Vec::new());
        }
        let payloads = self.payloads(dir, meta, matched.keys().copied()).await?;
        let mut hits = Vec::with_capacity(matched.len());
        for (doc_idx, id) in matched {
            let payload = payloads
                .get(&doc_idx)
                .with_context(|| format!("missing payload for doc {doc_idx}"))?;
            if payload.id != id {
                bail!("shard id/payload mismatch for doc {doc_idx}")
            }
            hits.push(ChunkLookupHit {
                id,
                text: payload.text.clone(),
                metadata: payload.metadata.clone(),
            });
        }
        hits.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(hits)
    }

    async fn retain_live_candidates(
        &self,
        dir: &Directory,
        meta: &ShardMeta,
        ranked: Vec<(u32, crate::search::fusion::Evidence, f32, f32)>,
        excluded_ids: &HashSet<String>,
        limit: usize,
    ) -> Result<Vec<(u32, crate::search::fusion::Evidence, f32, f32)>> {
        if excluded_ids.is_empty() {
            return Ok(ranked.into_iter().take(limit).collect());
        }

        let batch_size = limit.clamp(64, 4096);
        let mut live = Vec::with_capacity(limit);
        for batch in ranked.chunks(batch_size) {
            let ids = self.ids(dir, meta, batch.iter().map(|item| item.0)).await?;
            for item in batch {
                let id = ids
                    .get(&item.0)
                    .with_context(|| format!("missing id for doc {}", item.0))?;
                if !excluded_ids.contains(id) {
                    live.push(*item);
                    if live.len() == limit {
                        return Ok(live);
                    }
                }
            }
        }
        Ok(live)
    }

    async fn ids<I: IntoIterator<Item = u32>>(
        &self,
        dir: &Directory,
        meta: &ShardMeta,
        docs: I,
    ) -> Result<BTreeMap<u32, String>> {
        let wanted: BTreeSet<u32> = docs.into_iter().collect();
        let block_ids: BTreeSet<u32> = wanted.iter().map(|d| *d / meta.id_block_size).collect();
        let mut jobs = Vec::new();
        for block_id in block_ids {
            let me = self.clone();
            let dir = dir.clone();
            jobs.push(async move {
                me.block::<Vec<IdRecord>>(&dir, "ids", &block_id.to_string())
                    .await
            });
        }
        let blocks = stream::iter(jobs)
            .buffer_unordered(MAX_CONCURRENT_RANGE_READS)
            .try_collect::<Vec<_>>()
            .await?;
        let mut out = BTreeMap::new();
        for block in blocks {
            for record in block {
                validate_id_record(meta, &record)?;
                if wanted.contains(&record.doc_idx)
                    && out.insert(record.doc_idx, record.id).is_some()
                {
                    bail!("duplicate id record doc_idx")
                }
            }
        }
        Ok(out)
    }

    async fn lexical(
        &self,
        dir: &Directory,
        meta: &ShardMeta,
        terms: &BTreeSet<String>,
        stats: &RevisionStats,
    ) -> Result<Vec<(u32, f32)>> {
        let mut jobs = Vec::new();
        for term in terms {
            let Some(key) = meta.term_to_block.get(term) else {
                continue;
            };
            let me = self.clone();
            let dir = dir.clone();
            let key = key.clone();
            let term = term.clone();
            jobs.push(async move {
                let list: PostingList = me.block(&dir, "lex", &key).await?;
                Ok::<_, anyhow::Error>((term, list))
            });
        }
        let lists = stream::iter(jobs)
            .buffer_unordered(MAX_CONCURRENT_RANGE_READS)
            .try_collect::<Vec<_>>()
            .await?;
        let mut scores: BTreeMap<u32, f32> = BTreeMap::new();
        for (term, list) in lists {
            validate_posting_list(meta, &list)?;
            let Some(df) = stats.term_df.get(&term) else {
                continue;
            };
            for p in &list.postings {
                *scores.entry(p.doc_idx).or_default() += bm25(
                    p.tf,
                    p.doc_len,
                    *df,
                    stats.live_document_count as u32,
                    stats.average_document_length,
                    stats.scoring.bm25_k1,
                    stats.scoring.bm25_b,
                );
                if scores.len() > MAX_LEXICAL_ACCUMULATOR_DOCS {
                    bail!(
                        "lexical candidate accumulator exceeded safety limit of {MAX_LEXICAL_ACCUMULATOR_DOCS} documents; compact/shard the corpus or implement BMW/WAND for this workload"
                    )
                }
            }
        }
        Ok(scores
            .into_iter()
            .map(|(doc, s)| {
                (
                    doc,
                    stats
                        .scoring
                        .cold
                        .lexical
                        .evidence_llr(s, stats.scoring.base_rate),
                )
            })
            .collect())
    }

    async fn vector(
        &self,
        dir: &Directory,
        meta: &ShardMeta,
        q: &[f32],
        nprobe: usize,
        candidates: usize,
    ) -> Result<Vec<(u32, f32)>> {
        let q = normalize(q)?;
        let router: Router = self.block(dir, "router", "main").await?;
        validate_router(meta, &router)?;
        let probes = top_centroids(
            &q,
            &router.centroids,
            nprobe.max(1).min(router.centroids.len()).min(256),
        );
        let mut jobs = Vec::new();
        for p in probes {
            let me = self.clone();
            let dir = dir.clone();
            jobs.push(async move { me.block::<IvfList>(&dir, "ivf", &p.to_string()).await });
        }
        let lists = stream::iter(jobs)
            .buffer_unordered(MAX_CONCURRENT_RANGE_READS)
            .try_collect::<Vec<_>>()
            .await?;
        let mut best_by_doc = BTreeMap::<u32, f32>::new();
        for list in lists {
            for v in list.vectors {
                if v.doc_idx >= meta.document_count
                    || v.values.len() != meta.dimension as usize
                    || !v.scale.is_finite()
                    || v.scale <= 0.0
                {
                    bail!("corrupt quantized vector")
                }
                let score = approx_dot(&q, &v);
                if !score.is_finite() {
                    bail!("non-finite approximate vector score")
                }
                best_by_doc
                    .entry(v.doc_idx)
                    .and_modify(|current| {
                        if score > *current {
                            *current = score;
                        }
                    })
                    .or_insert(score);
            }
        }
        let mut scored: Vec<_> = best_by_doc.into_iter().collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(candidates.max(1));
        Ok(scored)
    }

    async fn exact_vector_evidence(
        &self,
        dir: &Directory,
        meta: &ShardMeta,
        q: &[f32],
        docs: Vec<u32>,
        stats: &RevisionStats,
    ) -> Result<Vec<(u32, f32)>> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }
        let q = Arc::new(normalize(q)?);
        let block_size = meta.exact_block_size;
        let mut by_block: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for d in docs {
            if d >= meta.document_count {
                bail!("candidate doc_idx out of bounds")
            }
            by_block.entry(d / block_size).or_default().push(d);
        }
        let mut jobs = Vec::new();
        for (block_id, wanted) in by_block {
            let me = self.clone();
            let dir = dir.clone();
            let dim = meta.dimension as usize;
            let q = q.clone();
            jobs.push(async move {
                let br = dir.get("exact", &block_id.to_string())?.clone();
                if br.len > MAX_COMPRESSED_BLOCK_BYTES {
                    bail!("compressed exact block too large: {}", br.len)
                }
                let bytes = me.range(br.offset, br.len).await?;
                let raw = decode_block_raw_async(br, bytes).await?;
                let stride = dim.checked_mul(2).context("exact vector stride overflow")?;
                if stride == 0 || raw.len() % stride != 0 {
                    bail!("corrupt exact vector block")
                }
                let mut out = Vec::new();
                for doc in wanted {
                    let local = (doc % block_size) as usize;
                    let start = local
                        .checked_mul(stride)
                        .context("exact vector offset overflow")?;
                    let end = start
                        .checked_add(stride)
                        .context("exact vector end overflow")?;
                    if end > raw.len() {
                        bail!("exact vector doc offset out of bounds")
                    }
                    let mut score = 0.0f32;
                    for j in 0..dim {
                        let bits = u16::from_le_bytes([raw[start + j * 2], raw[start + j * 2 + 1]]);
                        score += q[j] * half::f16::from_bits(bits).to_f32();
                    }
                    // FP16 roundoff can push a normalized cosine slightly outside
                    // [-1,1]. Clamp before calibration so the pruning bound stays sound.
                    out.push((doc, score.clamp(-1.0, 1.0)));
                }
                Ok::<_, anyhow::Error>(out)
            });
        }
        let blocks = stream::iter(jobs)
            .buffer_unordered(MAX_CONCURRENT_RANGE_READS)
            .try_collect::<Vec<_>>()
            .await?;
        let mut scored = Vec::new();
        for block in blocks {
            scored.extend(block)
        }
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(scored
            .into_iter()
            .map(|(doc, s)| {
                (
                    doc,
                    stats
                        .scoring
                        .cold
                        .vector
                        .evidence_llr(s, stats.scoring.base_rate),
                )
            })
            .collect())
    }

    async fn payloads<I: IntoIterator<Item = u32>>(
        &self,
        dir: &Directory,
        meta: &ShardMeta,
        docs: I,
    ) -> Result<BTreeMap<u32, PayloadRecord>> {
        let wanted: BTreeSet<u32> = docs.into_iter().collect();
        let block_ids: BTreeSet<u32> = wanted
            .iter()
            .map(|d| *d / meta.payload_block_size)
            .collect();
        let mut jobs = Vec::new();
        for b in block_ids {
            let me = self.clone();
            let dir = dir.clone();
            jobs.push(async move {
                me.block::<Vec<PayloadRecord>>(&dir, "payload", &b.to_string())
                    .await
            });
        }
        let blocks = stream::iter(jobs)
            .buffer_unordered(MAX_CONCURRENT_RANGE_READS)
            .try_collect::<Vec<_>>()
            .await?;
        let mut out = BTreeMap::new();
        for block in blocks {
            for p in block {
                validate_payload(meta, &p)?;
                if wanted.contains(&p.doc_idx) && out.insert(p.doc_idx, p).is_some() {
                    bail!("duplicate payload doc_idx")
                }
            }
        }
        Ok(out)
    }
}

fn validate_id_record(meta: &ShardMeta, record: &IdRecord) -> Result<()> {
    if record.doc_idx >= meta.document_count {
        bail!("id record doc_idx out of bounds")
    }
    if record.id.is_empty() || record.id.len() > crate::index::builder::MAX_CHUNK_ID_BYTES {
        bail!("invalid id record chunk id")
    }
    Ok(())
}

fn validate_posting_list(meta: &ShardMeta, list: &PostingList) -> Result<()> {
    if list.df as usize != list.postings.len() || list.df == 0 {
        bail!("corrupt posting-list document frequency")
    }
    if list.max_tf == 0 || list.min_doc_len == 0 {
        bail!("corrupt posting-list bounds")
    }
    if list.postings.iter().any(|posting| {
        posting.doc_idx >= meta.document_count || posting.tf == 0 || posting.doc_len == 0
    }) {
        bail!("corrupt posting entry")
    }
    let computed_max_tf = list
        .postings
        .iter()
        .map(|posting| posting.tf)
        .max()
        .unwrap_or(0);
    let computed_min_doc_len = list
        .postings
        .iter()
        .map(|posting| posting.doc_len)
        .min()
        .unwrap_or(0);
    if computed_max_tf != list.max_tf || computed_min_doc_len != list.min_doc_len {
        bail!("posting-list bounds do not match payload")
    }
    Ok(())
}

fn validate_router(meta: &ShardMeta, router: &Router) -> Result<()> {
    if router.dimension != meta.dimension || router.centroids.len() != meta.ivf_lists as usize {
        bail!("corrupt router shape")
    }
    if router.centroids.iter().any(|centroid| {
        centroid.len() != meta.dimension as usize || centroid.iter().any(|value| !value.is_finite())
    }) {
        bail!("corrupt router centroid")
    }
    Ok(())
}

fn validate_payload(meta: &ShardMeta, payload: &PayloadRecord) -> Result<()> {
    if payload.doc_idx >= meta.document_count {
        bail!("payload doc_idx out of bounds")
    }
    if payload.id.is_empty() || payload.id.len() > crate::index::builder::MAX_CHUNK_ID_BYTES {
        bail!("invalid payload chunk id")
    }
    if payload.text.len() > crate::index::builder::MAX_CHUNK_TEXT_BYTES {
        bail!("payload text exceeds limit")
    }
    let metadata_len = serde_json::to_vec(&payload.metadata)?.len();
    if metadata_len > crate::index::builder::MAX_CHUNK_METADATA_BYTES {
        bail!("payload metadata exceeds limit")
    }
    Ok(())
}

fn checked_query_terms(query: &str) -> Result<BTreeSet<String>> {
    let terms = query_terms(query);
    if terms.len() > MAX_QUERY_TERMS {
        bail!(
            "query expands to too many lexical terms ({} > {MAX_QUERY_TERMS})",
            terms.len()
        )
    }
    Ok(terms)
}

fn metadata_matches(
    metadata: &BTreeMap<String, serde_json::Value>,
    filters: &BTreeMap<String, serde_json::Value>,
) -> bool {
    filters.iter().all(|(k, v)| metadata.get(k) == Some(v))
}

fn weighted_interval_max(a: f32, b: f32, weight: f32) -> f32 {
    let lo = a.min(b);
    let hi = a.max(b);
    if weight >= 0.0 {
        hi * weight
    } else {
        lo * weight
    }
}

fn modality_upper_bound(a: f32, b: f32, weight: f32) -> f32 {
    // Hybrid retrieval is a union of lexical and vector candidate generators.
    // A candidate may therefore be absent from this modality entirely, which
    // contributes exactly zero evidence. If every calibrated match from this
    // modality is negative, using that negative value as the shard bound would
    // underestimate a candidate produced solely by the other modality and make
    // shard pruning unsound.
    weighted_interval_max(a, b, weight).max(0.0)
}

#[cfg(test)]
mod upper_bound_tests {
    use super::modality_upper_bound;

    #[test]
    fn absent_modality_is_part_of_the_upper_bound_domain() {
        assert_eq!(modality_upper_bound(-5.0, -2.0, 1.0), 0.0);
        assert_eq!(modality_upper_bound(1.0, 3.0, 2.0), 6.0);
    }
}
