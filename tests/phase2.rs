use anyhow::Context;
use bytes::Bytes;
use cairn_uqa::binary::{Header, HEADER_SIZE};
use cairn_uqa::index::builder::{build_shards, revision_stats_from_chunks, BuildOptions};
use cairn_uqa::manifest::{upload_content_addressed, upload_revision_stats, upload_tombstones, Catalog, RevisionManifest};
use cairn_uqa::model::{ChunkInput, GlobalScoringContext, ScoreDomain, SearchMode, SearchRequest};
use cairn_uqa::object_store::{LocalStore, ObjectStore, PutCondition};
use cairn_uqa::CairnRuntime;
use std::collections::BTreeMap;
use std::sync::Arc;

fn chunk(id: &str, text: &str, vector: Vec<f32>) -> ChunkInput {
    ChunkInput { id: id.into(), text: text.into(), vector, metadata: BTreeMap::new() }
}

fn request(query: &str, vector: Vec<f32>) -> SearchRequest {
    SearchRequest {
        query: query.into(), query_vector: vector, limit: 3, candidate_limit: 16,
        revision: None, filters: BTreeMap::new(),
        max_remote_bytes: 256 * 1024 * 1024, max_range_reads: 4096,
    }
}

async fn publish_fixture(
    store: Arc<dyn ObjectStore>,
    built_paths: Vec<std::path::PathBuf>,
    live_chunks: &[ChunkInput],
    tombstones: Vec<String>,
) -> anyhow::Result<RevisionManifest> {
    let mut shards = Vec::new();
    for path in built_paths {
        let bytes = Bytes::from(tokio::fs::read(path).await?);
        shards.push(upload_content_addressed(store.clone(), "objects/shards", bytes, "cairn").await?);
    }
    let stats = revision_stats_from_chunks(live_chunks, GlobalScoringContext::default(), BuildOptions::default().analyzer, "standard_cjk")?;
    let stats = upload_revision_stats(store.clone(), "objects/stats", &stats).await?;
    let mut ts = Vec::new();
    if !tombstones.is_empty() {
        ts.push(upload_tombstones(store.clone(), "objects/tombstones", tombstones).await?);
    }
    Ok(RevisionManifest {
        tenant: "t".into(), knowledge_base: "kb".into(), revision: 1, parent_revision: None,
        created_at_unix_ms: 1, embedding_provider: "external".into(), embedding_model: "test".into(), dimension: live_chunks[0].vector.len() as u32,
        shards, stats, tombstones: ts, uqa_bundle: None,
    })
}

#[tokio::test]
async fn local_store_conditional_put() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let s = LocalStore::new(td.path());
    assert!(s.put("a", Bytes::from_static(b"one"), PutCondition::CreateOnly).await?.is_some());
    assert!(s.put("a", Bytes::from_static(b"two"), PutCondition::CreateOnly).await?.is_none());
    let etag = s.head("a").await?.context("missing object a")?.etag.context("missing object etag")?;
    assert!(s.put("a", Bytes::from_static(b"two"), PutCondition::MatchEtag("wrong".into())).await?.is_none());
    assert!(s.put("a", Bytes::from_static(b"two"), PutCondition::MatchEtag(etag)).await?.is_some());
    let meta = s.head("a").await?.ok_or_else(|| anyhow::anyhow!("missing object"))?;
    assert_eq!(&s.get_range("a", 0, meta.size).await?[..], b"two");
    Ok(())
}

#[tokio::test]
async fn cold_search_uses_revision_stats_and_tombstones() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let built = td.path().join("built");
    let all = vec![
        chunk("rust-v1", "Rust systems programming", vec![1.0, 0.0, 0.0, 0.0]),
        chunk("search", "search engine retrieval", vec![0.0, 1.0, 0.0, 0.0]),
        chunk("object", "immutable object storage", vec![0.8, 0.0, 0.2, 0.0]),
        chunk("other", "unrelated material", vec![0.0, 0.0, 1.0, 0.0]),
    ];
    let live: Vec<_> = all.iter().filter(|c| c.id != "rust-v1").cloned().collect();
    let mut opts = BuildOptions::default(); opts.ivf_lists = 2;
    let outputs = build_shards(&all, &built, 2, &opts)?;
    let paths = outputs.into_iter().map(|(path, _)| path).collect();
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let manifest = publish_fixture(store.clone(), paths, &live, vec!["rust-v1".into()]).await?;
    Catalog::new(store.clone()).publish(&manifest).await?;
    let rt = CairnRuntime::new(store, td.path().join("cache"));
    let result = rt.search("t", "kb", &request("Rust", vec![1.0, 0.0, 0.0, 0.0])).await?;
    assert_eq!(result.mode, SearchMode::Cold);
    assert_eq!(result.score_domain, ScoreDomain::RevisionCalibratedLogOdds);
    assert!(result.approximate);
    assert!(result.hits.iter().all(|h| h.id != "rust-v1"));
    assert!(result.remote_bytes > 0 && result.range_reads > 0);
    Ok(())
}

#[tokio::test]
async fn catalog_rejects_stale_parent_and_unsafe_scope() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let chunks = vec![chunk("a", "alpha", vec![1.0])];
    let built = td.path().join("built");
    let outputs = build_shards(&chunks, &built, 1, &BuildOptions::default())?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let mut m1 = publish_fixture(store.clone(), outputs.into_iter().map(|x| x.0).collect(), &chunks, vec![]).await?;
    m1.knowledge_base = "k".into();
    let cat = Catalog::new(store.clone());
    cat.publish(&m1).await?;
    let mut m2 = m1.clone(); m2.revision = 2; m2.parent_revision = Some(1); m2.created_at_unix_ms = 2;
    cat.publish(&m2).await?;
    let mut stale = m2.clone(); stale.revision = 3; stale.parent_revision = Some(1); stale.created_at_unix_ms = 3;
    assert!(cat.publish(&stale).await.is_err());
    assert_eq!(cat.get_head("t", "k").await?.context("missing HEAD")?.0.revision, 2);
    assert!(Catalog::head_key("../escape", "k").is_err());
    assert!(Catalog::head_key("t", "a/b").is_err());
    Ok(())
}

#[tokio::test]
async fn manifest_directory_hash_detects_tampering() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let chunks = vec![chunk("a", "alpha beta", vec![1.0, 0.0]), chunk("b", "beta", vec![0.0, 1.0])];
    let built = td.path().join("built");
    let outputs = build_shards(&chunks, &built, 1, &BuildOptions::default())?;
    let bytes = Bytes::from(tokio::fs::read(&outputs[0].0).await?);
    let header = Header::decode(&bytes[..HEADER_SIZE])?;
    assert_ne!(header.directory_sha256_hex(), "0".repeat(64));
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let mut descriptor = upload_content_addressed(store.clone(), "objects/shards", bytes, "cairn").await?;
    descriptor.directory_sha256 = "0".repeat(64);
    let stats = revision_stats_from_chunks(&chunks, GlobalScoringContext::default(), BuildOptions::default().analyzer, "standard_cjk")?;
    let stats = upload_revision_stats(store.clone(), "objects/stats", &stats).await?;
    let manifest = RevisionManifest {
        tenant: "t".into(), knowledge_base: "kb".into(), revision: 1, parent_revision: None,
        created_at_unix_ms: 1, embedding_provider: "external".into(), embedding_model: "test".into(), dimension: 2,
        shards: vec![descriptor], stats, tombstones: vec![], uqa_bundle: None,
    };
    assert!(Catalog::new(store.clone()).publish(&manifest).await.is_err());
    assert!(Catalog::new(store).get_head("t", "kb").await?.is_none());
    Ok(())
}


#[test]
fn cold_and_warm_calibrations_are_independent() -> anyhow::Result<()> {
    use cairn_uqa::model::{Calibration, SignalCalibrations};
    let mut scoring = GlobalScoringContext::default();
    scoring.cold = SignalCalibrations {
        lexical: Calibration { slope: 1.0, intercept: 0.0 },
        vector: Calibration { slope: 1.0, intercept: 0.0 },
    };
    scoring.warm = SignalCalibrations {
        lexical: Calibration { slope: 2.0, intercept: -1.0 },
        vector: Calibration { slope: 3.0, intercept: -2.0 },
    };
    scoring.validate()?;
    let cold = scoring.cold.lexical.evidence_llr(1.0, scoring.base_rate);
    let warm = scoring.warm.lexical.evidence_llr(1.0, scoring.base_rate);
    assert_ne!(cold, warm);
    Ok(())
}

#[test]
fn corpus_digest_is_order_independent_and_content_sensitive() -> anyhow::Result<()> {
    use cairn_uqa::index::builder::corpus_sha256;
    let a = chunk("a", "alpha", vec![1.0, 0.0]);
    let b = chunk("b", "beta", vec![0.0, 1.0]);
    assert_eq!(corpus_sha256(&[a.clone(), b.clone()])?, corpus_sha256(&[b.clone(), a.clone()])?);
    let changed = chunk("b", "beta changed", vec![0.0, 1.0]);
    assert_ne!(corpus_sha256(&[a, b])?, corpus_sha256(&[chunk("a", "alpha", vec![1.0, 0.0]), changed])?);
    Ok(())
}

#[tokio::test]
async fn cold_search_enforces_range_read_budget() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let terms = (0..24).map(|i| format!("term{i}")).collect::<Vec<_>>();
    let text = terms.join(" ");
    let chunks = vec![chunk("a", &text, vec![1.0, 0.0])];
    let built = td.path().join("built");
    let outputs = build_shards(&chunks, &built, 1, &BuildOptions::default())?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let manifest = publish_fixture(store.clone(), outputs.into_iter().map(|x| x.0).collect(), &chunks, vec![]).await?;
    Catalog::new(store.clone()).publish(&manifest).await?;
    let rt = CairnRuntime::new(store, td.path().join("cache"));
    let mut req = request(&text, vec![]);
    req.max_range_reads = 16;
    assert!(rt.search("t", "kb", &req).await.is_err());
    Ok(())
}

#[tokio::test]
async fn promote_refuses_revision_with_missing_dependency() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let chunks = vec![chunk("a", "alpha", vec![1.0])];
    let built = td.path().join("built");
    let outputs = build_shards(&chunks, &built, 1, &BuildOptions::default())?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let m1 = publish_fixture(store.clone(), outputs.into_iter().map(|x| x.0).collect(), &chunks, vec![]).await?;
    let cat = Catalog::new(store.clone());
    cat.publish(&m1).await?;
    let mut m2 = m1.clone();
    m2.revision = 2;
    m2.parent_revision = Some(1);
    m2.created_at_unix_ms = 2;
    cat.publish(&m2).await?;
    store.delete(&m1.shards[0].key).await?;
    assert!(cat.promote("t", "kb", 1).await.is_err());
    assert_eq!(cat.get_head("t", "kb").await?.context("missing HEAD")?.0.revision, 2);
    Ok(())
}

#[tokio::test]
async fn orphan_revision_is_not_resolvable_as_committed_history() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let chunks = vec![chunk("a", "alpha", vec![1.0])];
    let built = td.path().join("built");
    let outputs = build_shards(&chunks, &built, 1, &BuildOptions::default())?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let m1 = publish_fixture(
        store.clone(),
        outputs.into_iter().map(|output| output.0).collect(),
        &chunks,
        vec![],
    )
    .await?;
    let catalog = Catalog::new(store.clone());
    catalog.publish(&m1).await?;

    let mut orphan = m1.clone();
    orphan.revision = 2;
    orphan.parent_revision = Some(1);
    orphan.created_at_unix_ms = 2;
    let key = Catalog::manifest_key("t", "kb", 2)?;
    let body = Bytes::from(serde_json::to_vec_pretty(&orphan)?);
    assert!(store.put(&key, body, PutCondition::CreateOnly).await?.is_some());

    assert!(store.head(&key).await?.is_some());
    assert!(catalog.resolve("t", "kb", Some(2)).await.is_err());
    assert_eq!(catalog.resolve("t", "kb", None).await?.revision, 1);
    Ok(())
}

#[test]
fn binary_directory_rejects_ambiguous_components() -> anyhow::Result<()> {
    use cairn_uqa::binary::ContainerWriter;
    use std::io::Cursor;

    let mut writer = ContainerWriter::new(Cursor::new(Vec::<u8>::new()))?;
    assert!(writer.add("lex:bad", "key", &1u32, 1).is_err());
    assert!(writer.add("lex", "bad:key", &1u32, 1).is_err());
    Ok(())
}

#[tokio::test]
async fn tombstones_do_not_consume_candidate_pool() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let built = td.path().join("built-tombstone-crowding");
    let mut all = (0..12)
        .map(|i| chunk(&format!("deleted-{i}"), "needle needle needle needle", vec![1.0, 0.0]))
        .collect::<Vec<_>>();
    let live = chunk("live", "needle", vec![0.0, 1.0]);
    all.push(live.clone());

    let mut options = BuildOptions::default();
    options.ivf_lists = 2;
    let outputs = build_shards(&all, &built, 1, &options)?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let tombstones = all
        .iter()
        .filter(|item| item.id.starts_with("deleted-"))
        .map(|item| item.id.clone())
        .collect::<Vec<_>>();
    let manifest = publish_fixture(
        store.clone(),
        outputs.into_iter().map(|output| output.0).collect(),
        std::slice::from_ref(&live),
        tombstones,
    )
    .await?;
    Catalog::new(store.clone()).publish(&manifest).await?;

    let runtime = CairnRuntime::new(store, td.path().join("cache"));
    let mut req = request("needle", vec![]);
    req.limit = 1;
    req.candidate_limit = 1;
    let result = runtime.search("t", "kb", &req).await?;
    assert_eq!(result.hits.first().map(|hit| hit.id.as_str()), Some("live"));
    Ok(())
}

#[tokio::test]
async fn orphan_revision_cannot_be_promoted_but_committed_revision_can_roll_forward() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let chunks = vec![chunk("a", "alpha", vec![1.0])];
    let built = td.path().join("built-commit-ledger");
    let outputs = build_shards(&chunks, &built, 1, &BuildOptions::default())?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let m1 = publish_fixture(
        store.clone(),
        outputs.into_iter().map(|output| output.0).collect(),
        &chunks,
        vec![],
    )
    .await?;
    let catalog = Catalog::new(store.clone());
    catalog.publish(&m1).await?;

    let mut m2 = m1.clone();
    m2.revision = 2;
    m2.parent_revision = Some(1);
    m2.created_at_unix_ms = 2;
    catalog.publish(&m2).await?;

    let mut orphan = m2.clone();
    orphan.revision = 3;
    orphan.parent_revision = Some(2);
    orphan.created_at_unix_ms = 3;
    let orphan_key = Catalog::manifest_key("t", "kb", 3)?;
    assert!(store
        .put(
            &orphan_key,
            Bytes::from(serde_json::to_vec_pretty(&orphan)?),
            PutCondition::CreateOnly,
        )
        .await?
        .is_some());
    assert!(catalog.promote("t", "kb", 3).await.is_err());

    catalog.promote("t", "kb", 1).await?;
    assert_eq!(catalog.get_head("t", "kb").await?.ok_or_else(|| anyhow::anyhow!("missing head"))?.0.revision, 1);
    catalog.promote("t", "kb", 2).await?;
    assert_eq!(catalog.get_head("t", "kb").await?.ok_or_else(|| anyhow::anyhow!("missing head"))?.0.revision, 2);
    Ok(())
}

#[tokio::test]
async fn automatic_revision_skips_immutable_manifests_after_rollback() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let chunks = vec![chunk("auto-revision", "revision allocation", vec![1.0, 0.0])];
    let outputs = build_shards(&chunks, &td.path().join("built-auto"), 1, &BuildOptions::default())?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let catalog = Catalog::new(store.clone());
    let m1 = publish_fixture(
        store,
        outputs.into_iter().map(|output| output.0).collect(),
        &chunks,
        vec![],
    )
    .await?;
    catalog.publish(&m1).await?;

    let mut m2 = m1.clone();
    m2.revision = 2;
    m2.parent_revision = Some(1);
    m2.created_at_unix_ms = 2;
    catalog.publish(&m2).await?;
    catalog.promote("t", "kb", 1).await?;

    assert_eq!(catalog.next_available_revision("t", "kb", Some(1)).await?, 3);
    Ok(())
}

#[tokio::test]
async fn publish_repairs_previous_head_commit_marker_before_advancing() -> anyhow::Result<()> {
    use cairn_uqa::manifest::Catalog;
    use cairn_uqa::object_store::{LocalStore, ObjectStore};

    let td = tempfile::tempdir()?;
    let chunks = vec![chunk("repair-v1", "rust repair marker", vec![1.0, 0.0])];
    let outputs = build_shards(&chunks, &td.path().join("built-r1"), 1, &BuildOptions::default())?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let catalog = Catalog::new(store.clone());
    let m1 = publish_fixture(store.clone(), outputs.into_iter().map(|x| x.0).collect(), &chunks, vec![]).await?;
    catalog.publish(&m1).await?;

    // Simulate the only crash window left after a successful HEAD CAS: current
    // HEAD is authoritative, but its post-CAS commit marker was never persisted.
    store.delete(&Catalog::commit_key("t", "kb", 1)?).await?;

    let chunks2 = vec![chunk("repair-v2", "rust repair marker two", vec![0.0, 1.0])];
    let outputs2 = build_shards(&chunks2, &td.path().join("built-r2"), 1, &BuildOptions::default())?;
    let mut m2 = publish_fixture(store.clone(), outputs2.into_iter().map(|x| x.0).collect(), &chunks2, vec![]).await?;
    m2.revision = 2;
    m2.parent_revision = Some(1);
    m2.created_at_unix_ms = 2;
    catalog.publish(&m2).await?;

    // Revision 1 became historical and must still be resolvable because publish
    // repaired its marker before replacing HEAD.
    assert_eq!(catalog.resolve("t", "kb", Some(1)).await?.revision, 1);
    assert_eq!(catalog.resolve("t", "kb", None).await?.revision, 2);
    Ok(())
}

#[tokio::test]
async fn promote_repairs_previous_head_commit_marker_before_moving_head() -> anyhow::Result<()> {
    use cairn_uqa::manifest::Catalog;
    use cairn_uqa::object_store::{LocalStore, ObjectStore};

    let td = tempfile::tempdir()?;
    let chunks = vec![chunk("promote-v1", "one", vec![1.0, 0.0])];
    let outputs = build_shards(&chunks, &td.path().join("built-r1"), 1, &BuildOptions::default())?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let catalog = Catalog::new(store.clone());
    let m1 = publish_fixture(store.clone(), outputs.into_iter().map(|x| x.0).collect(), &chunks, vec![]).await?;
    catalog.publish(&m1).await?;

    let chunks2 = vec![chunk("promote-v2", "two", vec![0.0, 1.0])];
    let outputs2 = build_shards(&chunks2, &td.path().join("built-r2"), 1, &BuildOptions::default())?;
    let mut m2 = publish_fixture(store.clone(), outputs2.into_iter().map(|x| x.0).collect(), &chunks2, vec![]).await?;
    m2.revision = 2;
    m2.parent_revision = Some(1);
    m2.created_at_unix_ms = 2;
    catalog.publish(&m2).await?;

    store.delete(&Catalog::commit_key("t", "kb", 2)?).await?;
    catalog.promote("t", "kb", 1).await?;
    assert_eq!(catalog.resolve("t", "kb", Some(2)).await?.revision, 2);
    assert_eq!(catalog.resolve("t", "kb", None).await?.revision, 1);
    Ok(())
}

#[test]
fn legacy_manifest_without_provider_is_external() -> anyhow::Result<()> {
    let raw = serde_json::json!({
        "tenant": "t",
        "knowledge_base": "kb",
        "revision": 1,
        "parent_revision": null,
        "created_at_unix_ms": 1,
        "embedding_model": "legacy-model",
        "dimension": 2,
        "shards": [],
        "stats": {"key":"objects/stats/x","sha256":"00".repeat(32),"size":1,"raw_sha256":"11".repeat(32)},
        "tombstones": [],
        "uqa_bundle": null
    });
    let manifest: RevisionManifest = serde_json::from_value(raw)?;
    assert_eq!(manifest.embedding_provider, "external");
    Ok(())
}

#[test]
fn header_rejects_nonzero_reserved_bytes() -> anyhow::Result<()> {
    use cairn_uqa::binary::{Header, FORMAT_VERSION, HEADER_SIZE};
    let header = Header {
        version: FORMAT_VERSION,
        dir_offset: HEADER_SIZE as u64,
        dir_len: 1,
        dir_crc32: 0,
        dir_sha256: [0; 32],
    };
    let mut encoded = header.encode();
    encoded[12] = 1;
    assert!(Header::decode(&encoded).is_err());
    Ok(())
}
