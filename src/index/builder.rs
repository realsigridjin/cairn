use crate::binary::{ContainerWriter, MAX_BLOCK_RAW};
use crate::index::lexical::build_postings_iter;
use crate::index::vector::train_ivf_iter;
use crate::index::{IdRecord, PayloadRecord, ShardMeta, TermBound};
use crate::model::{ChunkInput, GlobalScoringContext, RevisionStats};
use anyhow::{bail, Context, Result};
use half::f16;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub(crate) const MAX_CHUNK_ID_BYTES: usize = 4096;
pub(crate) const MAX_CHUNK_TEXT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_CHUNK_METADATA_BYTES: usize = 1024 * 1024;
const MAX_JSONL_LINE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildOptions {
    pub ivf_lists: usize,
    pub kmeans_iterations: usize,
    pub payload_block_size: usize,
    pub exact_block_size: usize,
    pub compression_level: i32,
    pub analyzer: String,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            ivf_lists: 64,
            kmeans_iterations: 8,
            payload_block_size: 128,
            exact_block_size: 256,
            compression_level: 3,
            analyzer: "cairn_standard_cjk_v1".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildReport {
    pub documents: usize,
    pub dimension: usize,
    pub terms: usize,
    pub ivf_lists: usize,
    pub output_bytes: u64,
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    output: &mut Vec<u8>,
    max_bytes: usize,
) -> Result<bool> {
    output.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!output.is_empty());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        let next_len = output
            .len()
            .checked_add(take)
            .context("JSONL line length overflow")?;
        if next_len > max_bytes {
            bail!("JSONL line exceeds {max_bytes} bytes")
        }
        output.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

pub fn read_jsonl_unvalidated(input: &Path) -> Result<Vec<ChunkInput>> {
    let f = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut reader = BufReader::new(f);
    let mut chunks = Vec::new();
    let mut buffer = Vec::with_capacity(16 * 1024);
    let mut line_no = 0usize;
    while read_bounded_line(&mut reader, &mut buffer, MAX_JSONL_LINE_BYTES)? {
        line_no = line_no
            .checked_add(1)
            .context("JSONL line number overflow")?;
        let line = buffer.strip_suffix(b"\n").unwrap_or(buffer.as_slice());
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let chunk: ChunkInput = serde_json::from_slice(line)
            .with_context(|| format!("invalid JSONL line {line_no}"))?;
        validate_chunk_payload(&chunk).with_context(|| format!("invalid JSONL line {line_no}"))?;
        chunks.push(chunk);
    }
    if chunks.is_empty() {
        bail!("no chunks")
    }
    Ok(chunks)
}

pub fn read_jsonl(input: &Path) -> Result<Vec<ChunkInput>> {
    let chunks = read_jsonl_unvalidated(input)?;
    validate_chunks(&chunks)?;
    Ok(chunks)
}

pub fn build_from_jsonl(
    input: &Path,
    output: &Path,
    options: &BuildOptions,
) -> Result<BuildReport> {
    let chunks = read_jsonl(input)?;
    build(&chunks, output, options)
}

pub fn build_shards_from_jsonl(
    input: &Path,
    output_dir: &Path,
    shard_count: usize,
    options: &BuildOptions,
) -> Result<Vec<(PathBuf, BuildReport)>> {
    let chunks = read_jsonl(input)?;
    build_shards(&chunks, output_dir, shard_count, options)
}

pub fn build_shards(
    chunks: &[ChunkInput],
    output_dir: &Path,
    shard_count: usize,
    options: &BuildOptions,
) -> Result<Vec<(PathBuf, BuildReport)>> {
    let refs = chunks.iter().collect::<Vec<_>>();
    build_shards_refs(&refs, output_dir, shard_count, options)
}

pub fn build_shards_refs(
    chunks: &[&ChunkInput],
    output_dir: &Path,
    shard_count: usize,
    options: &BuildOptions,
) -> Result<Vec<(PathBuf, BuildReport)>> {
    validate_chunk_refs(chunks)?;
    validate_options(options)?;
    if shard_count == 0 {
        bail!("shard_count must be positive")
    }
    std::fs::create_dir_all(output_dir)?;
    let actual = shard_count.min(chunks.len());
    let mut buckets: Vec<Vec<&ChunkInput>> = (0..actual).map(|_| Vec::new()).collect();
    for &chunk in chunks {
        let idx = stable_bucket(&chunk.id, actual)?;
        buckets[idx].push(chunk);
    }
    let mut out = Vec::new();
    for (i, bucket) in buckets.into_iter().enumerate() {
        if bucket.is_empty() {
            continue;
        }
        let path = output_dir.join(format!("shard-{i:04}.cairn"));
        let report = build_refs(&bucket, &path, options)?;
        out.push((path, report));
    }
    Ok(out)
}

pub fn build(chunks: &[ChunkInput], output: &Path, options: &BuildOptions) -> Result<BuildReport> {
    validate_chunks(chunks)?;
    validate_options(options)?;
    let refs: Vec<&ChunkInput> = chunks.iter().collect();
    build_refs(&refs, output, options)
}

fn build_refs(
    chunks: &[&ChunkInput],
    output: &Path,
    options: &BuildOptions,
) -> Result<BuildReport> {
    let dim = chunks
        .first()
        .context("cannot build an empty shard")?
        .vector
        .len();
    let exact_raw_bytes = options
        .exact_block_size
        .checked_mul(dim)
        .and_then(|value| value.checked_mul(2))
        .context("exact block byte-size overflow")?;
    if u64::try_from(exact_raw_bytes).context("exact block byte-size exceeds u64")? > MAX_BLOCK_RAW
    {
        bail!("exact_block_size × dimension exceeds the immutable block raw-size limit")
    }
    let (postings, _avg_doc_len, _) =
        build_postings_iter(chunks.iter().map(|chunk| chunk.text.as_str()))?;
    let (router, ivf_lists) = train_ivf_iter(
        chunks.iter().map(|chunk| chunk.vector.as_slice()),
        options.ivf_lists,
        options.kmeans_iterations,
    )?;

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    let mut writer = ContainerWriter::new(temporary)?;
    writer.add("router", "main", &router, options.compression_level)?;

    let mut term_to_block = BTreeMap::new();
    let mut term_bounds = BTreeMap::new();
    for (term, list) in &postings {
        let key = hex_term(term);
        writer.add("lex", &key, list, options.compression_level)?;
        term_to_block.insert(term.clone(), key);
        term_bounds.insert(
            term.clone(),
            TermBound {
                max_tf: list.max_tf,
                min_doc_len: list.min_doc_len,
            },
        );
    }

    for (i, list) in ivf_lists.iter().enumerate() {
        writer.add("ivf", &i.to_string(), list, options.compression_level)?;
    }

    for (block_idx, block) in chunks.chunks(options.payload_block_size).enumerate() {
        let ids = block
            .iter()
            .enumerate()
            .map(|(offset, chunk)| {
                let absolute = block_idx
                    .checked_mul(options.payload_block_size)
                    .and_then(|base| base.checked_add(offset))
                    .context("id document index overflow")?;
                Ok(IdRecord {
                    doc_idx: u32::try_from(absolute).context("id document index exceeds u32")?,
                    id: chunk.id.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        writer.add(
            "ids",
            &block_idx.to_string(),
            &ids,
            options.compression_level,
        )?;
    }

    for (block_idx, block) in chunks.chunks(options.payload_block_size).enumerate() {
        let payloads = block
            .iter()
            .enumerate()
            .map(|(offset, chunk)| {
                let absolute = block_idx
                    .checked_mul(options.payload_block_size)
                    .and_then(|base| base.checked_add(offset))
                    .context("payload document index overflow")?;
                Ok(PayloadRecord {
                    doc_idx: u32::try_from(absolute)
                        .context("payload document index exceeds u32")?,
                    id: chunk.id.clone(),
                    text: chunk.text.clone(),
                    metadata: chunk.metadata.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        writer.add(
            "payload",
            &block_idx.to_string(),
            &payloads,
            options.compression_level,
        )?;
    }

    for (block_idx, block) in chunks.chunks(options.exact_block_size).enumerate() {
        let mut raw = Vec::with_capacity(block.len().saturating_mul(dim).saturating_mul(2));
        for c in block {
            let norm = crate::index::vector::normalize(&c.vector)?;
            for x in norm {
                raw.extend_from_slice(&f16::from_f32(x).to_bits().to_le_bytes());
            }
        }
        writer.add_raw(
            "exact",
            &block_idx.to_string(),
            &raw,
            options.compression_level,
        )?;
    }

    let meta = ShardMeta {
        format_version: crate::binary::FORMAT_VERSION,
        document_count: u32::try_from(chunks.len()).context("too many documents in one shard")?,
        dimension: u32::try_from(dim).context("vector dimension too large")?,
        payload_block_size: u32::try_from(options.payload_block_size)
            .context("payload block size too large")?,
        id_block_size: u32::try_from(options.payload_block_size)
            .context("id block size too large")?,
        exact_block_size: u32::try_from(options.exact_block_size)
            .context("exact block size too large")?,
        ivf_lists: u32::try_from(ivf_lists.len()).context("too many IVF lists")?,
        term_to_block,
        term_bounds,
        analyzer: options.analyzer.clone(),
    };
    writer.add("meta", "main", &meta, options.compression_level)?;
    let temporary = writer.finish()?;
    temporary.as_file().sync_all()?;
    temporary.persist(output).map_err(|error| error.error)?;
    let output_bytes = std::fs::metadata(output)?.len();
    Ok(BuildReport {
        documents: chunks.len(),
        dimension: dim,
        terms: postings.len(),
        ivf_lists: ivf_lists.len(),
        output_bytes,
    })
}

pub fn revision_stats_from_chunks(
    chunks: &[ChunkInput],
    scoring: GlobalScoringContext,
    analyzer: impl Into<String>,
    warm_analyzer: impl Into<String>,
) -> Result<RevisionStats> {
    let refs = chunks.iter().collect::<Vec<_>>();
    revision_stats_from_chunk_refs(&refs, scoring, analyzer, warm_analyzer)
}

pub fn revision_stats_from_chunk_refs(
    chunks: &[&ChunkInput],
    scoring: GlobalScoringContext,
    analyzer: impl Into<String>,
    warm_analyzer: impl Into<String>,
) -> Result<RevisionStats> {
    validate_chunk_refs(chunks)?;
    scoring.validate()?;
    let (postings, avg_doc_len, _) =
        build_postings_iter(chunks.iter().map(|chunk| chunk.text.as_str()))?;
    let stats = RevisionStats {
        schema_version: 3,
        live_document_count: u64::try_from(chunks.len())
            .context("live document count exceeds u64")?,
        average_document_length: avg_doc_len,
        term_df: postings
            .into_iter()
            .map(|(term, list)| (term, list.df))
            .collect(),
        corpus_sha256: corpus_sha256_refs(chunks)?,
        analyzer: analyzer.into(),
        warm_analyzer: warm_analyzer.into(),
        scoring,
    };
    stats.validate()?;
    Ok(stats)
}

pub fn write_revision_stats_json(
    chunks: &[ChunkInput],
    scoring: GlobalScoringContext,
    analyzer: impl Into<String>,
    warm_analyzer: impl Into<String>,
    output: &Path,
) -> Result<RevisionStats> {
    let stats = revision_stats_from_chunks(chunks, scoring, analyzer, warm_analyzer)?;
    crate::config::atomic_write(output, &serde_json::to_vec_pretty(&stats)?)?;
    Ok(stats)
}

pub fn corpus_sha256(chunks: &[ChunkInput]) -> Result<String> {
    let refs = chunks.iter().collect::<Vec<_>>();
    corpus_sha256_refs(&refs)
}

pub fn corpus_sha256_refs(chunks: &[&ChunkInput]) -> Result<String> {
    validate_chunk_refs(chunks)?;
    let mut ordered = chunks.to_vec();
    ordered.sort_by(|a, b| a.id.cmp(&b.id));
    let mut h = Sha256::new();
    h.update(b"CAIRN-CORPUS-V1\0");
    for c in ordered {
        hash_len_bytes(&mut h, c.id.as_bytes())?;
        hash_len_bytes(&mut h, c.text.as_bytes())?;
        let metadata = serde_json::to_vec(&c.metadata)?;
        hash_len_bytes(&mut h, &metadata)?;
        let vector_len = u64::try_from(c.vector.len()).context("vector length overflow")?;
        h.update(vector_len.to_le_bytes());
        for value in &c.vector {
            h.update(value.to_bits().to_le_bytes());
        }
    }
    let digest = h.finalize();
    let mut out = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(out)
}

fn hash_len_bytes(h: &mut Sha256, bytes: &[u8]) -> Result<()> {
    let len = u64::try_from(bytes.len()).context("corpus field length overflow")?;
    h.update(len.to_le_bytes());
    h.update(bytes);
    Ok(())
}

fn validate_options(options: &BuildOptions) -> Result<()> {
    if options.payload_block_size == 0 || options.exact_block_size == 0 {
        bail!("block sizes must be positive")
    }
    if options.payload_block_size > 1_000_000 || options.exact_block_size > 1_000_000 {
        bail!("block sizes must be <= 1,000,000 records")
    }
    if !(-7..=22).contains(&options.compression_level) {
        bail!("compression_level must be in -7..=22")
    }
    if options.ivf_lists == 0 || options.ivf_lists > 65_536 {
        bail!("ivf_lists must be in 1..=65536")
    }
    if options.kmeans_iterations == 0 || options.kmeans_iterations > 1_000 {
        bail!("kmeans_iterations must be in 1..=1000")
    }
    if options.analyzer.is_empty() || options.analyzer.len() > 128 {
        bail!("invalid analyzer identifier")
    }
    Ok(())
}

fn validate_chunk_payload(chunk: &ChunkInput) -> Result<()> {
    if chunk.id.is_empty() || chunk.id.len() > MAX_CHUNK_ID_BYTES {
        bail!("invalid chunk id")
    }
    if chunk.text.trim().is_empty() {
        bail!("chunk {} text must not be empty", chunk.id)
    }
    if chunk.text.len() > MAX_CHUNK_TEXT_BYTES {
        bail!(
            "chunk {} text exceeds {} bytes",
            chunk.id,
            MAX_CHUNK_TEXT_BYTES
        )
    }
    let metadata_len = serde_json::to_vec(&chunk.metadata)?.len();
    if metadata_len > MAX_CHUNK_METADATA_BYTES {
        bail!(
            "chunk {} metadata exceeds {} bytes",
            chunk.id,
            MAX_CHUNK_METADATA_BYTES
        )
    }
    if chunk.vector.iter().any(|value| !value.is_finite()) {
        bail!("chunk {} vector contains non-finite values", chunk.id)
    }
    Ok(())
}

pub fn validate_chunks(chunks: &[ChunkInput]) -> Result<()> {
    let refs = chunks.iter().collect::<Vec<_>>();
    validate_chunk_refs(&refs)
}

pub fn validate_chunk_refs(chunks: &[&ChunkInput]) -> Result<()> {
    if chunks.is_empty() {
        bail!("no chunks")
    }
    if chunks.len() > u32::MAX as usize {
        bail!("too many chunks")
    }
    let dim = chunks.first().context("no chunks")?.vector.len();
    if dim == 0 {
        bail!("zero vector dimension")
    }
    if dim > 65_536 {
        bail!("vector dimension too large")
    }
    if chunks.iter().any(|c| c.vector.len() != dim) {
        bail!("inconsistent vector dimensions")
    }
    if chunks
        .iter()
        .flat_map(|c| c.vector.iter())
        .any(|x| !x.is_finite())
    {
        bail!("vectors must contain only finite values")
    }
    if chunks.iter().any(|chunk| {
        let norm_sq = chunk.vector.iter().map(|value| value * value).sum::<f32>();
        !norm_sq.is_finite() || norm_sq <= 1e-24
    }) {
        bail!("vectors must have a finite non-zero norm")
    }
    let mut seen = std::collections::BTreeSet::new();
    for &chunk in chunks {
        validate_chunk_payload(chunk)?;
        if !seen.insert(&chunk.id) {
            bail!("duplicate chunk id {}", chunk.id)
        }
    }
    Ok(())
}

fn stable_bucket(id: &str, buckets: usize) -> Result<usize> {
    if buckets == 0 {
        bail!("bucket count must be positive")
    }
    let d = Sha256::digest(id.as_bytes());
    let mut first = [0u8; 8];
    first.copy_from_slice(&d[0..8]);
    let modulus = u64::try_from(buckets).context("bucket count does not fit u64")?;
    usize::try_from(u64::from_le_bytes(first) % modulus).context("bucket index does not fit usize")
}

fn hex_term(term: &str) -> String {
    use sha2::{Digest, Sha256};
    // Use the full digest. A truncated block key makes a hash collision an
    // index-corruption event because two distinct terms could address the same
    // immutable block.
    let digest = Sha256::digest(term.as_bytes());
    let mut out = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
