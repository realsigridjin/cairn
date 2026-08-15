use crate::index::builder::{build_shards_refs, revision_stats_from_chunk_refs, validate_chunks, BuildOptions, BuildReport};
use crate::model::{ChunkInput, GlobalScoringContext, RevisionStats};
use anyhow::{bail, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const MAX_TOMBSTONE_TEXT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct CompactionReport {
    pub input_chunks: usize,
    pub tombstoned: usize,
    pub output_chunks: usize,
    pub shards: Vec<(PathBuf, BuildReport)>,
    pub stats: RevisionStats,
}

/// Build a compacted full-replacement index from the canonical live corpus.
///
/// CAIRN intentionally does **not** rebuild production shards by decoding the
/// normalized FP16 rerank vectors stored in older index shards. Those are a
/// serving representation and would introduce irreversible numeric drift on
/// each compaction. The canonical chunk/vector corpus (or a deterministic
/// regeneration of it from source documents) is the compaction input.
pub fn compact_canonical_chunks(
    chunks: &[ChunkInput],
    tombstones: &BTreeSet<String>,
    output_dir: &Path,
    shard_count: usize,
    options: &BuildOptions,
    scoring: GlobalScoringContext,
    warm_analyzer: &str,
) -> Result<CompactionReport> {
    validate_chunks(chunks)?;
    if shard_count == 0 { bail!("compaction shard_count must be positive") }
    let input_chunks = chunks.len();
    let compacted: Vec<&ChunkInput> = chunks.iter()
        .filter(|chunk| !tombstones.contains(&chunk.id))
        .collect();
    let tombstoned = input_chunks.saturating_sub(compacted.len());
    if compacted.is_empty() { bail!("compaction removed all chunks; empty revisions are unsupported") }
    let stats = revision_stats_from_chunk_refs(&compacted, scoring, options.analyzer.clone(), warm_analyzer)?;
    let shards = build_shards_refs(&compacted, output_dir, shard_count, options)?;
    Ok(CompactionReport { input_chunks, tombstoned, output_chunks: compacted.len(), shards, stats })
}

pub fn read_tombstone_lines(path: &Path) -> Result<BTreeSet<String>> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut text = String::new();
    file.by_ref()
        .take(MAX_TOMBSTONE_TEXT_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_TOMBSTONE_TEXT_BYTES {
        bail!("tombstone text file exceeds {MAX_TOMBSTONE_TEXT_BYTES} bytes")
    }
    let mut out = BTreeSet::new();
    for raw in text.lines() {
        let id = raw.trim();
        if id.is_empty() || id.starts_with('#') { continue }
        if id.len() > 4096 { bail!("tombstone id exceeds 4096 bytes") }
        out.insert(id.to_owned());
    }
    Ok(out)
}
