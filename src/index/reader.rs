use crate::binary::{decode_block, decode_block_raw, decode_directory, Directory, Header, HEADER_SIZE, MAX_BLOCK_RAW, MAX_COMPRESSED_BLOCK_BYTES, MAX_DIRECTORY_BYTES};
use crate::index::{PayloadRecord, ShardMeta};
use crate::model::ChunkInput;
use anyhow::{bail, Context, Result};
use half::f16;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;


pub fn read_shard_meta(path: &Path) -> Result<ShardMeta> {
    let mut f = File::open(path).with_context(|| format!("open shard {}", path.display()))?;
    let object_size = f.metadata()?.len();
    let mut hb = [0u8; HEADER_SIZE];
    f.read_exact(&mut hb)?;
    let header = Header::decode(&hb)?;
    if header.dir_len == 0 || header.dir_len > MAX_DIRECTORY_BYTES { bail!("invalid directory size") }
    let dir_end = header.dir_offset.checked_add(header.dir_len).context("directory range overflow")?;
    if dir_end != object_size { bail!("directory must terminate the shard object") }
    f.seek(SeekFrom::Start(header.dir_offset))?;
    let mut db = vec![0u8; usize::try_from(header.dir_len).context("directory too large")?];
    f.read_exact(&mut db)?;
    let dir = decode_directory(header, &db)?;
    dir.validate_layout(object_size, header.dir_offset, header.dir_len)?;
    let meta: ShardMeta = read_block(&mut f, &dir, "meta", "main")?;
    meta.validate()?;
    crate::index::validate_directory_schema(&dir, &meta)?;
    Ok(meta)
}

/// Reconstructs chunk payloads and normalized FP16 vectors from a local
/// immutable CAIRN shard. Used by compaction and offline validation.
pub fn read_all_chunks(path: &Path) -> Result<(Vec<ChunkInput>, ShardMeta)> {
    let mut f = File::open(path).with_context(|| format!("open shard {}", path.display()))?;
    let object_size = f.metadata()?.len();
    let mut hb = [0u8; HEADER_SIZE];
    f.read_exact(&mut hb)?;
    let header = Header::decode(&hb)?;
    if header.dir_len == 0 || header.dir_len > MAX_DIRECTORY_BYTES { bail!("invalid directory size") }
    let dir_len = usize::try_from(header.dir_len).context("directory too large")?;
    let dir_end = header.dir_offset.checked_add(header.dir_len).context("directory range overflow")?;
    if dir_end != object_size { bail!("directory must terminate the shard object") }
    f.seek(SeekFrom::Start(header.dir_offset))?;
    let mut db = vec![0u8; dir_len];
    f.read_exact(&mut db)?;
    let dir = decode_directory(header, &db)?;
    dir.validate_layout(object_size, header.dir_offset, header.dir_len)?;
    let meta: ShardMeta = read_block(&mut f, &dir, "meta", "main")?;
    meta.validate()?;
    crate::index::validate_directory_schema(&dir, &meta)?;

    let mut payloads: BTreeMap<u32, PayloadRecord> = BTreeMap::new();
    for block_id in 0..div_ceil(meta.document_count, meta.payload_block_size) {
        let block: Vec<PayloadRecord> = read_block(&mut f, &dir, "payload", &block_id.to_string())?;
        for p in block {
            if p.doc_idx >= meta.document_count { bail!("payload doc_idx out of bounds") }
            if payloads.insert(p.doc_idx, p).is_some() { bail!("duplicate payload doc_idx") }
        }
    }

    let dim = meta.dimension as usize;
    let mut vectors: BTreeMap<u32, Vec<f32>> = BTreeMap::new();
    let exact_bs = meta.exact_block_size;
    for block_id in 0..div_ceil(meta.document_count, exact_bs) {
        let br = dir.get("exact", &block_id.to_string())?.clone();
        if br.len > MAX_COMPRESSED_BLOCK_BYTES { bail!("compressed exact block too large") }
        let len = usize::try_from(br.len).context("compressed exact block too large")?;
        f.seek(SeekFrom::Start(br.offset))?;
        let mut compressed = vec![0u8; len];
        f.read_exact(&mut compressed)?;
        let raw = decode_block_raw(&br, &compressed, MAX_BLOCK_RAW)?;
        let stride = dim.checked_mul(2).context("vector stride overflow")?;
        if stride == 0 || raw.len() % stride != 0 { bail!("corrupt exact block") }
        for local in 0..(raw.len() / stride) {
            let doc_idx = block_id.checked_mul(exact_bs).and_then(|x| x.checked_add(local as u32)).context("doc index overflow")?;
            if doc_idx >= meta.document_count { break }
            let start = local * stride;
            let mut v = Vec::with_capacity(dim);
            for j in 0..dim {
                let i = start + j * 2;
                v.push(f16::from_bits(u16::from_le_bytes([raw[i], raw[i + 1]])).to_f32());
            }
            if vectors.insert(doc_idx, v).is_some() { bail!("duplicate vector doc_idx") }
        }
    }

    let mut chunks = Vec::with_capacity(meta.document_count as usize);
    for doc_idx in 0..meta.document_count {
        let p = payloads.remove(&doc_idx).with_context(|| format!("missing payload for doc {doc_idx}"))?;
        let vector = vectors.remove(&doc_idx).with_context(|| format!("missing vector for doc {doc_idx}"))?;
        chunks.push(ChunkInput { id: p.id, text: p.text, vector, metadata: p.metadata });
    }
    Ok((chunks, meta))
}

pub fn read_header(path: &Path) -> Result<(Header, u64)> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    let mut hb = [0u8; HEADER_SIZE];
    f.read_exact(&mut hb)?;
    Ok((Header::decode(&hb)?, size))
}

fn read_block<T: serde::de::DeserializeOwned>(f: &mut File, dir: &Directory, kind: &str, key: &str) -> Result<T> {
    let br = dir.get(kind, key)?.clone();
    if br.len > MAX_COMPRESSED_BLOCK_BYTES { bail!("compressed block too large") }
    let len = usize::try_from(br.len).context("compressed block too large")?;
    f.seek(SeekFrom::Start(br.offset))?;
    let mut compressed = vec![0u8; len];
    f.read_exact(&mut compressed)?;
    decode_block(&br, &compressed, MAX_BLOCK_RAW)
}

fn div_ceil(n: u32, d: u32) -> u32 {
    if n == 0 { 0 } else { 1 + (n - 1) / d }
}
