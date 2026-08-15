use anyhow::{bail, Context, Result};
use bincode::Options;
use crc32fast::Hasher as Crc32;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};

pub const MAGIC: &[u8; 8] = b"CAIRN004";
pub const FORMAT_VERSION: u32 = 4;
pub const HEADER_SIZE: usize = 96;
pub const MAX_DIRECTORY_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_COMPRESSED_BLOCK_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_BLOCK_RAW: u64 = 128 * 1024 * 1024;
pub const MAX_DIRECTORY_BLOCKS: usize = 1_000_000;
const MAX_BLOCK_COMPONENT_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockRef {
    pub kind: String,
    pub key: String,
    pub offset: u64,
    pub len: u64,
    pub raw_len: u64,
    pub crc32: u32,
    /// SHA-256 of the compressed block. CRC32 catches accidental corruption;
    /// SHA-256 ties each range-read block to the authenticated directory.
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Directory {
    pub blocks: BTreeMap<String, BlockRef>,
}

impl Directory {
    pub fn id(kind: &str, key: &str) -> String {
        format!("{kind}:{key}")
    }

    pub fn get(&self, kind: &str, key: &str) -> Result<&BlockRef> {
        self.blocks
            .get(&Self::id(kind, key))
            .with_context(|| format!("missing block {kind}:{key}"))
    }

    pub fn validate_layout(&self, object_size: u64, dir_offset: u64, dir_len: u64) -> Result<()> {
        if self.blocks.len() > MAX_DIRECTORY_BLOCKS {
            bail!("directory contains too many blocks")
        }
        let dir_end = dir_offset
            .checked_add(dir_len)
            .context("directory range overflow")?;
        if dir_offset < HEADER_SIZE as u64 || dir_end != object_size {
            bail!("invalid directory placement: offset={dir_offset} len={dir_len} object={object_size}");
        }
        let mut ranges = Vec::with_capacity(self.blocks.len());
        for (id, block) in &self.blocks {
            validate_block_component("kind", &block.kind)?;
            validate_block_component("key", &block.key)?;
            if Directory::id(&block.kind, &block.key) != *id {
                bail!("directory key mismatch for {id}")
            }
            if block.len == 0 || block.len > MAX_COMPRESSED_BLOCK_BYTES {
                bail!("invalid compressed block length for {id}")
            }
            if block.raw_len > MAX_BLOCK_RAW {
                bail!("invalid raw block length for {id}")
            }
            if block.sha256.len() != 64 || !block.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("invalid block SHA-256 for {id}")
            }
            let end = block
                .offset
                .checked_add(block.len)
                .context("block range overflow")?;
            if block.offset < HEADER_SIZE as u64 || end > dir_offset {
                bail!("block {id} outside payload region")
            }
            ranges.push((block.offset, end, id));
        }
        ranges.sort_by_key(|x| x.0);
        for pair in ranges.windows(2) {
            if pair[0].1 > pair[1].0 {
                bail!("overlapping blocks {} and {}", pair[0].2, pair[1].2)
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub version: u32,
    pub dir_offset: u64,
    pub dir_len: u64,
    pub dir_crc32: u32,
    pub dir_sha256: [u8; 32],
}

impl Header {
    pub fn encode(self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[0..8].copy_from_slice(MAGIC);
        out[8..12].copy_from_slice(&self.version.to_le_bytes());
        out[16..24].copy_from_slice(&self.dir_offset.to_le_bytes());
        out[24..32].copy_from_slice(&self.dir_len.to_le_bytes());
        out[32..36].copy_from_slice(&self.dir_crc32.to_le_bytes());
        out[40..72].copy_from_slice(&self.dir_sha256);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_SIZE {
            bail!("short CAIRN header")
        }
        if &buf[0..8] != MAGIC {
            bail!("bad CAIRN magic")
        }
        if buf[12..16]
            .iter()
            .chain(&buf[36..40])
            .chain(&buf[72..96])
            .any(|byte| *byte != 0)
        {
            bail!("non-zero reserved CAIRN header bytes")
        }
        let version = u32::from_le_bytes(buf[8..12].try_into().context("version bytes")?);
        if version != FORMAT_VERSION {
            bail!("unsupported CAIRN format version {version}")
        }
        let mut dir_sha256 = [0u8; 32];
        dir_sha256.copy_from_slice(&buf[40..72]);
        Ok(Self {
            version,
            dir_offset: u64::from_le_bytes(buf[16..24].try_into().context("dir offset bytes")?),
            dir_len: u64::from_le_bytes(buf[24..32].try_into().context("dir length bytes")?),
            dir_crc32: u32::from_le_bytes(buf[32..36].try_into().context("dir crc bytes")?),
            dir_sha256,
        })
    }

    pub fn directory_sha256_hex(&self) -> String {
        hex_digest(&self.dir_sha256)
    }
}

pub struct ContainerWriter<W: Write + Seek> {
    writer: W,
    dir: Directory,
}

impl<W: Write + Seek> ContainerWriter<W> {
    pub fn new(mut writer: W) -> Result<Self> {
        writer.write_all(&[0u8; HEADER_SIZE])?;
        Ok(Self {
            writer,
            dir: Directory::default(),
        })
    }

    pub fn add<T: Serialize>(
        &mut self,
        kind: &str,
        key: &str,
        value: &T,
        compression_level: i32,
    ) -> Result<()> {
        let raw = bincode_options(MAX_BLOCK_RAW).serialize(value)?;
        self.add_raw(kind, key, &raw, compression_level)
    }

    pub fn add_raw(
        &mut self,
        kind: &str,
        key: &str,
        raw: &[u8],
        compression_level: i32,
    ) -> Result<()> {
        validate_block_component("kind", kind)?;
        validate_block_component("key", key)?;
        if self.dir.blocks.len() >= MAX_DIRECTORY_BLOCKS {
            bail!("directory block count exceeds format limit")
        }
        if raw.len() as u64 > MAX_BLOCK_RAW {
            bail!("raw block exceeds format limit")
        }
        let id = Directory::id(kind, key);
        if self.dir.blocks.contains_key(&id) {
            bail!("duplicate block {id}")
        }
        let compressed = zstd::stream::encode_all(raw, compression_level)?;
        if compressed.len() as u64 > MAX_COMPRESSED_BLOCK_BYTES {
            bail!("compressed block exceeds format limit")
        }
        let offset = self.writer.stream_position()?;
        self.writer.write_all(&compressed)?;
        let mut crc = Crc32::new();
        crc.update(&compressed);
        self.dir.blocks.insert(
            id,
            BlockRef {
                kind: kind.to_string(),
                key: key.to_string(),
                offset,
                len: compressed.len() as u64,
                raw_len: raw.len() as u64,
                crc32: crc.finalize(),
                sha256: hex_sha256(&compressed),
            },
        );
        Ok(())
    }

    pub fn finish(mut self) -> Result<W> {
        let dir_offset = self.writer.stream_position()?;
        let dir_bytes = serde_json::to_vec(&self.dir)?;
        if dir_bytes.len() as u64 > MAX_DIRECTORY_BYTES {
            bail!("directory exceeds format limit")
        }
        let mut crc = Crc32::new();
        crc.update(&dir_bytes);
        let dir_crc32 = crc.finalize();
        let digest = Sha256::digest(&dir_bytes);
        let mut dir_sha256 = [0u8; 32];
        dir_sha256.copy_from_slice(&digest);
        self.writer.write_all(&dir_bytes)?;
        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(
            &Header {
                version: FORMAT_VERSION,
                dir_offset,
                dir_len: dir_bytes.len() as u64,
                dir_crc32,
                dir_sha256,
            }
            .encode(),
        )?;
        self.writer.flush()?;
        Ok(self.writer)
    }
}

pub fn decode_directory(header: Header, bytes: &[u8]) -> Result<Directory> {
    if header.dir_len == 0 || header.dir_len > MAX_DIRECTORY_BYTES {
        bail!("invalid directory length")
    }
    if bytes.len() != usize::try_from(header.dir_len).context("directory length too large")? {
        bail!("directory length mismatch")
    }
    let mut crc = Crc32::new();
    crc.update(bytes);
    if crc.finalize() != header.dir_crc32 {
        bail!("directory checksum mismatch")
    }
    let digest = Sha256::digest(bytes);
    if digest[..] != header.dir_sha256[..] {
        bail!("directory SHA-256 mismatch")
    }
    Ok(serde_json::from_slice(bytes)?)
}

pub fn decode_block_raw(block: &BlockRef, compressed: &[u8], max_raw_len: u64) -> Result<Vec<u8>> {
    verify_compressed_block(block, compressed, max_raw_len)?;
    let mut decoder = zstd::stream::read::Decoder::new(compressed)?;
    let mut raw =
        Vec::with_capacity(usize::try_from(block.raw_len).context("raw block too large")?);
    decoder
        .by_ref()
        .take(max_raw_len.saturating_add(1))
        .read_to_end(&mut raw)?;
    if raw.len() as u64 != block.raw_len {
        bail!("decompressed block length mismatch")
    }
    Ok(raw)
}

pub fn decode_block<T: DeserializeOwned>(
    block: &BlockRef,
    compressed: &[u8],
    max_raw_len: u64,
) -> Result<T> {
    let raw = decode_block_raw(block, compressed, max_raw_len)?;
    Ok(bincode_options(max_raw_len).deserialize(&raw)?)
}

fn verify_compressed_block(block: &BlockRef, compressed: &[u8], max_raw_len: u64) -> Result<()> {
    if compressed.len() as u64 != block.len {
        bail!("block length mismatch")
    }
    if block.len > MAX_COMPRESSED_BLOCK_BYTES {
        bail!("compressed block exceeds limit")
    }
    if block.raw_len > max_raw_len || block.raw_len > MAX_BLOCK_RAW {
        bail!("block exceeds decompression limit")
    }
    let mut crc = Crc32::new();
    crc.update(compressed);
    if crc.finalize() != block.crc32 {
        bail!("block checksum mismatch")
    }
    if hex_sha256(compressed) != block.sha256 {
        bail!("block SHA-256 mismatch")
    }
    Ok(())
}

fn validate_block_component(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_BLOCK_COMPONENT_BYTES
        || value.contains(':')
        || value.contains('\0')
    {
        bail!("invalid block {label}")
    }
    Ok(())
}

fn bincode_options(limit: u64) -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(limit)
        .reject_trailing_bytes()
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    hex_digest(&Sha256::digest(bytes))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}
