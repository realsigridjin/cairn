pub mod builder;
pub mod lexical;
pub mod reader;
pub mod vector;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermBound {
    pub max_tf: u16,
    pub min_doc_len: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardMeta {
    pub format_version: u32,
    pub document_count: u32,
    pub dimension: u32,
    pub payload_block_size: u32,
    pub id_block_size: u32,
    pub exact_block_size: u32,
    pub ivf_lists: u32,
    pub term_to_block: BTreeMap<String, String>,
    pub term_bounds: BTreeMap<String, TermBound>,
    pub analyzer: String,
}

impl ShardMeta {
    pub fn validate(&self) -> Result<()> {
        if self.format_version != crate::binary::FORMAT_VERSION {
            bail!("shard metadata format mismatch")
        }
        if self.document_count == 0 {
            bail!("empty shards are unsupported")
        }
        if self.dimension == 0 || self.dimension > 65_536 {
            bail!("invalid shard dimension")
        }
        if self.payload_block_size == 0 || self.id_block_size == 0 || self.exact_block_size == 0 {
            bail!("invalid shard block size")
        }
        if self.ivf_lists == 0 || self.ivf_lists > self.document_count {
            bail!("invalid IVF list count")
        }
        if self.analyzer.is_empty() || self.analyzer.len() > 128 {
            bail!("invalid analyzer identifier")
        }
        if self.term_to_block.len() != self.term_bounds.len() {
            bail!("term map/bound cardinality mismatch")
        }
        if self.term_to_block.len() > 10_000_000 {
            bail!("too many lexical terms in one shard")
        }
        let mut lexical_block_keys = BTreeSet::new();
        for (term, block_key) in &self.term_to_block {
            if term.is_empty() || term.len() > 4096 {
                bail!("invalid lexical term")
            }
            if block_key.is_empty() || block_key.len() > 128 {
                bail!("invalid lexical block key")
            }
            if !lexical_block_keys.insert(block_key) {
                bail!("multiple lexical terms reference the same block")
            }
            let bound = self
                .term_bounds
                .get(term)
                .ok_or_else(|| anyhow::anyhow!("missing term bound"))?;
            if bound.max_tf == 0 || bound.min_doc_len == 0 {
                bail!("invalid lexical term bound")
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadRecord {
    pub doc_idx: u32,
    pub id: String,
    pub text: String,
    #[serde(with = "metadata_json")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

mod metadata_json {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        metadata: &BTreeMap<String, serde_json::Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serde_json::to_string(metadata)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, serde_json::Value>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        serde_json::from_str(&encoded).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdRecord {
    pub doc_idx: u32,
    pub id: String,
}

pub fn validate_directory_schema(dir: &crate::binary::Directory, meta: &ShardMeta) -> Result<()> {
    dir.get("router", "main")?;
    dir.get("meta", "main")?;
    let payload_blocks = div_ceil(meta.document_count, meta.payload_block_size);
    let id_blocks = div_ceil(meta.document_count, meta.id_block_size);
    let exact_blocks = div_ceil(meta.document_count, meta.exact_block_size);
    for block_id in 0..payload_blocks {
        dir.get("payload", &block_id.to_string())?;
    }
    for block_id in 0..id_blocks {
        dir.get("ids", &block_id.to_string())?;
    }
    for block_id in 0..exact_blocks {
        dir.get("exact", &block_id.to_string())?;
    }
    for list_id in 0..meta.ivf_lists {
        dir.get("ivf", &list_id.to_string())?;
    }
    for key in meta.term_to_block.values() {
        dir.get("lex", key)?;
    }
    let payload_blocks = usize::try_from(payload_blocks)
        .map_err(|_| anyhow::anyhow!("payload block count overflow"))?;
    let id_blocks =
        usize::try_from(id_blocks).map_err(|_| anyhow::anyhow!("id block count overflow"))?;
    let exact_blocks =
        usize::try_from(exact_blocks).map_err(|_| anyhow::anyhow!("exact block count overflow"))?;
    let ivf_lists =
        usize::try_from(meta.ivf_lists).map_err(|_| anyhow::anyhow!("IVF list count overflow"))?;
    let expected_blocks = 2usize
        .checked_add(payload_blocks)
        .and_then(|count| count.checked_add(id_blocks))
        .and_then(|count| count.checked_add(exact_blocks))
        .and_then(|count| count.checked_add(ivf_lists))
        .and_then(|count| count.checked_add(meta.term_to_block.len()))
        .ok_or_else(|| anyhow::anyhow!("directory block count overflow"))?;
    if dir.blocks.len() != expected_blocks {
        bail!(
            "directory schema mismatch: found {} blocks, expected {expected_blocks}",
            dir.blocks.len(),
        )
    }
    Ok(())
}

fn div_ceil(n: u32, d: u32) -> u32 {
    if n == 0 {
        0
    } else {
        1 + (n - 1) / d
    }
}

#[cfg(test)]
mod tests {
    use super::PayloadRecord;
    use std::collections::BTreeMap;

    #[test]
    fn payload_record_metadata_round_trips_through_bincode() -> anyhow::Result<()> {
        let metadata = BTreeMap::from([
            ("lang".to_owned(), serde_json::Value::from("ko")),
            ("page".to_owned(), serde_json::Value::from(3)),
        ]);
        let record = PayloadRecord {
            doc_idx: 7,
            id: "doc-v1-c0".to_owned(),
            text: "검색 결과".to_owned(),
            metadata,
        };
        let encoded = bincode::serialize(&record)?;
        let decoded: PayloadRecord = bincode::deserialize(&encoded)?;
        assert_eq!(
            decoded.metadata.get("lang"),
            Some(&serde_json::Value::from("ko"))
        );
        assert_eq!(
            decoded.metadata.get("page"),
            Some(&serde_json::Value::from(3))
        );
        Ok(())
    }
}
