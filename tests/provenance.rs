//! Pins the two publication safety branches around embedding provenance:
//!
//! 1. Ordinary/delta publication that reuses any parent shard object key must
//!    keep rejecting embedding provider/model/dimension changes.
//! 2. An explicit full shard replacement (snapshot / `--replace-shards`) must
//!    be allowed to re-label embedding provenance even when a deterministically
//!    rebuilt shard is byte-identical to the parent shard and therefore keeps
//!    the same content-addressed object key.

use anyhow::Context;
use bytes::Bytes;
use cairn_uqa::index::builder::{build_shards, revision_stats_from_chunks, BuildOptions};
use cairn_uqa::manifest::{
    upload_content_addressed, upload_revision_stats, Catalog, RevisionManifest,
};
use cairn_uqa::model::{ChunkInput, GlobalScoringContext};
use cairn_uqa::object_store::{LocalStore, ObjectStore};
use std::collections::BTreeMap;
use std::sync::Arc;

fn chunk(id: &str, text: &str, vector: Vec<f32>) -> ChunkInput {
    ChunkInput {
        id: id.into(),
        text: text.into(),
        vector,
        metadata: BTreeMap::new(),
    }
}

/// Build and publish a root revision with provider `external`, returning the
/// catalog and the committed parent manifest.
async fn publish_root(store: Arc<dyn ObjectStore>) -> anyhow::Result<(Catalog, RevisionManifest)> {
    let chunks = vec![
        chunk("doc-a", "alpha object storage", vec![1.0, 0.0]),
        chunk("doc-b", "beta rust retrieval", vec![0.0, 1.0]),
        chunk("doc-c", "gamma search index", vec![1.0, 1.0]),
        chunk("doc-d", "delta catalog publish", vec![0.5, 0.5]),
    ];
    let staging = tempfile::tempdir()?;
    let outputs = build_shards(
        &chunks,
        &staging.path().join("built"),
        1,
        &BuildOptions {
            ivf_lists: 1,
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
        &chunks,
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
        embedding_provider: "external".into(),
        embedding_model: "bge-m3-external".into(),
        dimension: 2,
        shards,
        stats,
        tombstones: vec![],
        uqa_bundle: None,
    };
    let catalog = Catalog::new(store);
    catalog.publish(&manifest).await?;
    Ok((catalog, manifest))
}

/// A revision 2 whose shard descriptors are byte-identical to the parent's.
/// This is exactly what a deterministic snapshot rebuild produces when the
/// corpus is unchanged: the content-addressed keys overlap the parent even
/// though the caller performed a full replacement.
fn rebuilt_replacement(parent: &RevisionManifest) -> RevisionManifest {
    let mut replacement = parent.clone();
    replacement.revision = 2;
    replacement.parent_revision = Some(parent.revision);
    replacement.embedding_provider = "openrouter".into();
    replacement.embedding_model = "qwen/qwen3-embedding-8b".into();
    replacement
}

#[tokio::test]
async fn conservative_publish_rejects_identity_change_when_reusing_parent_shard_keys(
) -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let (catalog, parent) = publish_root(store).await?;

    let replacement = rebuilt_replacement(&parent);
    assert!(
        replacement
            .shards
            .iter()
            .any(|s| parent.shards.iter().any(|p| p.key == s.key)),
        "fixture must reuse parent shard object keys"
    );

    let err = match catalog.publish(&replacement).await {
        Ok(()) => {
            anyhow::bail!("ordinary publish reusing a parent shard must reject provenance changes")
        }
        Err(err) => err,
    };
    assert!(
        err.to_string()
            .contains("cannot change embedding model/dimension while inheriting parent shards"),
        "unexpected error: {err:#}"
    );

    // The failed publication must not have moved HEAD.
    let (head, _) = catalog
        .get_head("t", "kb")
        .await?
        .context("HEAD must exist")?;
    assert_eq!(head.revision, 1);
    Ok(())
}

#[tokio::test]
async fn explicit_full_replacement_allows_identity_change_with_identical_shard_keys(
) -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let (catalog, parent) = publish_root(store).await?;

    let replacement = rebuilt_replacement(&parent);
    assert_eq!(
        replacement
            .shards
            .iter()
            .map(|s| s.key.as_str())
            .collect::<Vec<_>>(),
        parent
            .shards
            .iter()
            .map(|s| s.key.as_str())
            .collect::<Vec<_>>(),
        "deterministic rebuild must keep the parent's shard object keys"
    );

    catalog.publish_full_replacement(&replacement).await?;

    // HEAD moved atomically to the relabeled revision, and the parent remains
    // resolvable through the commit ledger.
    let (head, _) = catalog
        .get_head("t", "kb")
        .await?
        .context("HEAD must exist")?;
    assert_eq!(head.revision, 2);
    let resolved = catalog.resolve("t", "kb", None).await?;
    assert_eq!(resolved.embedding_provider, "openrouter");
    assert_eq!(resolved.embedding_model, "qwen/qwen3-embedding-8b");
    assert_eq!(resolved.shards.len(), parent.shards.len());
    let parent_resolved = catalog.resolve("t", "kb", Some(1)).await?;
    assert_eq!(parent_resolved.embedding_provider, "external");
    Ok(())
}
