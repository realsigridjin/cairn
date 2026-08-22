use bytes::Bytes;
use cairn_uqa::index::builder::{
    build_shards, corpus_sha256, revision_stats_from_chunks, BuildOptions,
};
use cairn_uqa::manifest::{
    upload_content_addressed, upload_revision_stats, upload_tombstones, Catalog, RevisionManifest,
};
use cairn_uqa::model::{ChunkInput, ChunkLookupOrder, ChunkLookupRequest, GlobalScoringContext};
use cairn_uqa::object_store::{LocalStore, ObjectStore};
use cairn_uqa::CairnRuntime;
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

fn session_chunk(id: &str, doc_type: &str, seq_start: Option<serde_json::Value>) -> ChunkInput {
    let mut metadata = BTreeMap::new();
    metadata.insert("doc_type".into(), serde_json::Value::from(doc_type));
    if let Some(seq_start) = seq_start {
        metadata.insert("seq_start".into(), seq_start);
    }
    ChunkInput {
        id: id.into(),
        text: format!("window text for {id}"),
        vector: vec![1.0, 0.0],
        metadata,
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

async fn publish_fixture(
    store: Arc<dyn ObjectStore>,
    built_paths: Vec<std::path::PathBuf>,
    live_chunks: &[ChunkInput],
    tombstones: Vec<String>,
) -> anyhow::Result<RevisionManifest> {
    let mut shards = Vec::new();
    for path in built_paths {
        let bytes = Bytes::from(tokio::fs::read(path).await?);
        shards
            .push(upload_content_addressed(store.clone(), "objects/shards", bytes, "cairn").await?);
    }
    let stats = revision_stats_from_chunks(
        live_chunks,
        GlobalScoringContext::default(),
        BuildOptions::default().analyzer,
        "standard_cjk",
    )?;
    let stats = upload_revision_stats(store.clone(), "objects/stats", &stats).await?;
    let mut ts = Vec::new();
    if !tombstones.is_empty() {
        ts.push(upload_tombstones(store.clone(), "objects/tombstones", tombstones).await?);
    }
    Ok(RevisionManifest {
        tenant: "t".into(),
        knowledge_base: "kb".into(),
        revision: 1,
        parent_revision: None,
        created_at_unix_ms: 1,
        embedding_provider: "external".into(),
        embedding_model: "test".into(),
        dimension: live_chunks[0].vector.len() as u32,
        shards,
        stats,
        tombstones: ts,
        uqa_bundle: None,
    })
}

#[test]
fn lookup_request_rejects_invalid_prefix_limit_and_budgets() {
    let mut req = lookup_request("doc-");
    assert!(req.validate().is_ok());

    req.id_prefix = String::new();
    assert!(req.validate().is_err(), "empty prefix must be rejected");

    req.id_prefix = "doc-\u{0007}".into();
    assert!(req.validate().is_err(), "control bytes must be rejected");

    req.id_prefix = "doc-\n".into();
    assert!(req.validate().is_err(), "newline must be rejected");

    req.id_prefix = "x".repeat(4097);
    assert!(req.validate().is_err(), "oversized prefix must be rejected");

    req = lookup_request("doc-");
    req.limit = 0;
    assert!(req.validate().is_err(), "limit 0 must be rejected");
    req.limit = 1001;
    assert!(req.validate().is_err(), "limit > 1000 must be rejected");

    req = lookup_request("doc-");
    req.max_remote_bytes = 1024;
    assert!(req.validate().is_err(), "sub-1MiB byte budget rejected");
    req.max_remote_bytes = 5 * 1024 * 1024 * 1024;
    assert!(req.validate().is_err(), "over-4GiB byte budget rejected");

    req = lookup_request("doc-");
    req.max_range_reads = 15;
    assert!(req.validate().is_err(), "sub-16 read budget rejected");
    req.max_range_reads = 100_001;
    assert!(req.validate().is_err(), "over-100000 read budget rejected");
}

#[test]
fn lookup_request_denies_unknown_fields() -> anyhow::Result<()> {
    let raw = serde_json::json!({
        "id_prefix": "doc-",
        "limit": 10,
        "revision": null,
        "max_remote_bytes": 1048576,
        "max_range_reads": 16,
        "unexpected": true
    });
    assert!(serde_json::from_value::<ChunkLookupRequest>(raw).is_err());
    let minimal = serde_json::json!({"id_prefix": "doc-"});
    let parsed: ChunkLookupRequest = serde_json::from_value(minimal)?;
    assert!(parsed.validate().is_ok());
    assert!(parsed.revision.is_none());
    Ok(())
}

#[test]
fn lookup_request_order_by_is_typed_and_defaults_to_id() -> anyhow::Result<()> {
    let parsed: ChunkLookupRequest = serde_json::from_value(serde_json::json!({
        "id_prefix": "doc-"
    }))?;
    assert_eq!(parsed.order_by, ChunkLookupOrder::Id);

    let parsed: ChunkLookupRequest = serde_json::from_value(serde_json::json!({
        "id_prefix": "doc-",
        "order_by": "seq_start"
    }))?;
    assert_eq!(parsed.order_by, ChunkLookupOrder::SeqStart);

    let parsed: ChunkLookupRequest = serde_json::from_value(serde_json::json!({
        "id_prefix": "doc-",
        "order_by": "id"
    }))?;
    assert_eq!(parsed.order_by, ChunkLookupOrder::Id);

    for invalid in [
        serde_json::json!({"id_prefix": "doc-", "order_by": "chronological"}),
        serde_json::json!({"id_prefix": "doc-", "order_by": "SEQ_START"}),
        serde_json::json!({"id_prefix": "doc-", "order_by": 3}),
        serde_json::json!({"id_prefix": "doc-", "order_by": null}),
    ] {
        assert!(
            serde_json::from_value::<ChunkLookupRequest>(invalid.clone()).is_err(),
            "unknown order_by value must be rejected by typed serde: {invalid}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn prefix_lookup_spans_shards_sorts_globally_and_truncates() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let mut all: Vec<ChunkInput> = (0..12)
        .map(|i| {
            chunk(
                &format!("doc-{i:02}"),
                &format!("window text for document {i}"),
                vec![1.0, 0.0],
            )
        })
        .collect();
    all.push(chunk("misc-a", "unrelated", vec![0.0, 1.0]));
    all.push(chunk("misc-b", "also unrelated", vec![0.0, 1.0]));
    let outputs = build_shards(
        &all,
        &td.path().join("built"),
        4,
        &BuildOptions {
            ivf_lists: 2,
            ..BuildOptions::default()
        },
    )?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let manifest = publish_fixture(
        store.clone(),
        outputs.into_iter().map(|output| output.0).collect(),
        &all,
        vec![],
    )
    .await?;
    assert!(
        manifest.shards.len() >= 2,
        "fixture must span multiple shards"
    );
    Catalog::new(store.clone()).publish(&manifest).await?;
    let rt = CairnRuntime::new(store, td.path().join("cache"));

    let result = rt
        .chunks_by_id_prefix("t", "kb", &lookup_request("doc-"))
        .await?;
    assert_eq!(result.revision, 1);
    assert_eq!(result.embedding_provider, "external");
    assert_eq!(result.embedding_model, "test");
    assert_eq!(result.dimension, 2);
    assert_eq!(
        result.corpus_sha256,
        corpus_sha256(&all)?,
        "response must pin the revision live-corpus digest"
    );
    assert_eq!(result.hits.len(), 12);
    let ids: Vec<&str> = result.hits.iter().map(|hit| hit.id.as_str()).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "hits must be globally sorted by id");
    assert_eq!(ids.first(), Some(&"doc-00"));
    assert_eq!(ids.last(), Some(&"doc-11"));
    assert!(result.remote_bytes > 0 && result.range_reads > 0);

    let mut truncated = lookup_request("doc-");
    truncated.limit = 5;
    let result = rt.chunks_by_id_prefix("t", "kb", &truncated).await?;
    assert_eq!(result.hits.len(), 5);
    assert_eq!(
        result.hits.last().map(|hit| hit.id.as_str()),
        Some("doc-04"),
        "truncation keeps the globally first ids"
    );
    Ok(())
}

#[tokio::test]
async fn prefix_lookup_returns_full_text_windows_and_metadata() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let mut with_meta = chunk(
        "handbook-017",
        "a longer retrieval window with several sentences of context preserved verbatim",
        vec![1.0, 0.0],
    );
    with_meta
        .metadata
        .insert("source".into(), serde_json::Value::from("handbook"));
    with_meta
        .metadata
        .insert("page".into(), serde_json::Value::from(17));
    let all = vec![
        with_meta,
        chunk("handbook-018", "another window", vec![0.0, 1.0]),
        chunk("other-1", "outside the prefix", vec![1.0, 1.0]),
    ];
    let outputs = build_shards(
        &all,
        &td.path().join("built"),
        2,
        &BuildOptions {
            ivf_lists: 2,
            ..BuildOptions::default()
        },
    )?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let manifest = publish_fixture(
        store.clone(),
        outputs.into_iter().map(|output| output.0).collect(),
        &all,
        vec![],
    )
    .await?;
    Catalog::new(store.clone()).publish(&manifest).await?;
    let rt = CairnRuntime::new(store, td.path().join("cache"));

    let result = rt
        .chunks_by_id_prefix("t", "kb", &lookup_request("handbook-"))
        .await?;
    assert_eq!(result.hits.len(), 2);
    let hit = &result.hits[0];
    assert_eq!(hit.id, "handbook-017");
    assert_eq!(
        hit.text,
        "a longer retrieval window with several sentences of context preserved verbatim"
    );
    assert_eq!(
        hit.metadata.get("source"),
        Some(&serde_json::Value::from("handbook"))
    );
    assert_eq!(hit.metadata.get("page"), Some(&serde_json::Value::from(17)));
    assert!(result.hits[1].metadata.is_empty());
    Ok(())
}

#[tokio::test]
async fn prefix_lookup_excludes_tombstoned_ids() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let all = vec![
        chunk("doc-keep", "still live", vec![1.0, 0.0]),
        chunk("doc-deleted", "removed content", vec![0.0, 1.0]),
    ];
    let live: Vec<_> = all
        .iter()
        .filter(|c| c.id != "doc-deleted")
        .cloned()
        .collect();
    let outputs = build_shards(
        &all,
        &td.path().join("built"),
        2,
        &BuildOptions {
            ivf_lists: 2,
            ..BuildOptions::default()
        },
    )?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let manifest = publish_fixture(
        store.clone(),
        outputs.into_iter().map(|output| output.0).collect(),
        &live,
        vec!["doc-deleted".into()],
    )
    .await?;
    Catalog::new(store.clone()).publish(&manifest).await?;
    let rt = CairnRuntime::new(store, td.path().join("cache"));

    let result = rt
        .chunks_by_id_prefix("t", "kb", &lookup_request("doc-"))
        .await?;
    let ids: Vec<&str> = result.hits.iter().map(|hit| hit.id.as_str()).collect();
    assert_eq!(ids, vec!["doc-keep"]);
    assert_eq!(result.corpus_sha256, corpus_sha256(&live)?);
    Ok(())
}

#[tokio::test]
async fn prefix_lookup_with_absent_prefix_returns_no_hits() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let all = vec![
        chunk("doc-1", "alpha", vec![1.0, 0.0]),
        chunk("doc-2", "beta", vec![0.0, 1.0]),
    ];
    let outputs = build_shards(
        &all,
        &td.path().join("built"),
        1,
        &BuildOptions {
            ivf_lists: 2,
            ..BuildOptions::default()
        },
    )?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let manifest = publish_fixture(
        store.clone(),
        outputs.into_iter().map(|output| output.0).collect(),
        &all,
        vec![],
    )
    .await?;
    Catalog::new(store.clone()).publish(&manifest).await?;
    let rt = CairnRuntime::new(store, td.path().join("cache"));

    let result = rt
        .chunks_by_id_prefix("t", "kb", &lookup_request("missing-"))
        .await?;
    assert!(result.hits.is_empty());
    assert_eq!(result.revision, 1);
    assert!(result.remote_bytes > 0, "id blocks were still scanned");
    Ok(())
}

#[tokio::test]
async fn prefix_lookup_enforces_range_read_budget() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let all: Vec<ChunkInput> = (0..32)
        .map(|i| chunk(&format!("doc-{i:02}"), "budget probe", vec![1.0, 0.0]))
        .collect();
    let outputs = build_shards(
        &all,
        &td.path().join("built"),
        8,
        &BuildOptions {
            ivf_lists: 2,
            ..BuildOptions::default()
        },
    )?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let manifest = publish_fixture(
        store.clone(),
        outputs.into_iter().map(|output| output.0).collect(),
        &all,
        vec![],
    )
    .await?;
    assert!(
        manifest.shards.len() >= 5,
        "budget fixture needs enough shards to exceed the floor budget"
    );
    Catalog::new(store.clone()).publish(&manifest).await?;
    let rt = CairnRuntime::new(store, td.path().join("cache"));

    let mut req = lookup_request("doc-");
    req.max_range_reads = 16;
    let Err(error) = rt.chunks_by_id_prefix("t", "kb", &req).await else {
        anyhow::bail!("a full multi-shard id scan must exceed 16 range reads")
    };
    assert!(
        error.to_string().contains("budget"),
        "expected a budget error, got: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn seq_start_order_preserves_earliest_windows_under_truncation() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    // Chunk ids deliberately disagree with chronological order so an id-sorted
    // truncation would keep the wrong windows.
    let all = vec![
        session_chunk("s:abc:meta", "session_meta", None),
        session_chunk(
            "s:abc:w-a",
            "session_window",
            Some(serde_json::Value::from(40)),
        ),
        session_chunk(
            "s:abc:w-b",
            "session_window",
            Some(serde_json::Value::from(5)),
        ),
        session_chunk(
            "s:abc:w-c",
            "session_window",
            Some(serde_json::Value::from(20)),
        ),
        // Persisted session_messages are valid transcript windows and must
        // participate in global seq_start ordering alongside session_window.
        session_chunk(
            "s:abc:m-a",
            "session_messages",
            Some(serde_json::Value::from(10)),
        ),
        session_chunk(
            "s:abc:m-b",
            "session_messages",
            Some(serde_json::Value::from(-2)),
        ),
        session_chunk("s:abc:m-c", "session_messages", None),
        session_chunk(
            "s:abc:w-d",
            "session_window",
            Some(serde_json::Value::from(-1)),
        ),
        session_chunk(
            "s:abc:w-e",
            "session_window",
            Some(serde_json::Value::from(1.5)),
        ),
        session_chunk("s:abc:w-f", "session_window", None),
        chunk("s:abc:other", "not a session chunk", vec![0.0, 1.0]),
    ];
    let outputs = build_shards(
        &all,
        &td.path().join("built"),
        3,
        &BuildOptions {
            ivf_lists: 2,
            ..BuildOptions::default()
        },
    )?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let manifest = publish_fixture(
        store.clone(),
        outputs.into_iter().map(|output| output.0).collect(),
        &all,
        vec![],
    )
    .await?;
    Catalog::new(store.clone()).publish(&manifest).await?;
    let rt = CairnRuntime::new(store, td.path().join("cache"));

    let mut seq = lookup_request("s:abc:");
    seq.order_by = ChunkLookupOrder::SeqStart;
    let result = rt.chunks_by_id_prefix("t", "kb", &seq).await?;
    let ids: Vec<&str> = result.hits.iter().map(|hit| hit.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "s:abc:meta", // lineage first
            "s:abc:w-b",  // seq_start 5
            "s:abc:m-a",  // seq_start 10 (session_messages)
            "s:abc:w-c",  // seq_start 20
            "s:abc:w-a",  // seq_start 40
            // malformed/other chunks last, id as deterministic tie-break
            "s:abc:m-b",
            "s:abc:m-c",
            "s:abc:other",
            "s:abc:w-d",
            "s:abc:w-e",
            "s:abc:w-f",
        ]
    );

    // Truncation happens after chronological ordering, so the earliest valid
    // windows survive a bounded request without fetching every text.
    seq.limit = 3;
    let result = rt.chunks_by_id_prefix("t", "kb", &seq).await?;
    let ids: Vec<&str> = result.hits.iter().map(|hit| hit.id.as_str()).collect();
    assert_eq!(ids, vec!["s:abc:meta", "s:abc:w-b", "s:abc:m-a"]);

    // The generic default remains plain id order with the same limit.
    let mut by_id = lookup_request("s:abc:");
    by_id.limit = 3;
    let result = rt.chunks_by_id_prefix("t", "kb", &by_id).await?;
    let ids: Vec<&str> = result.hits.iter().map(|hit| hit.id.as_str()).collect();
    assert_eq!(ids, vec!["s:abc:m-a", "s:abc:m-b", "s:abc:m-c"]);
    Ok(())
}

#[tokio::test]
async fn prefix_lookup_pins_requested_historical_revision() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    let catalog = Catalog::new(store.clone());

    let rev1 = vec![chunk("rev-a", "revision one content", vec![1.0, 0.0])];
    let outputs1 = build_shards(
        &rev1,
        &td.path().join("built-r1"),
        1,
        &BuildOptions {
            ivf_lists: 1,
            ..BuildOptions::default()
        },
    )?;
    let m1 = publish_fixture(
        store.clone(),
        outputs1.into_iter().map(|output| output.0).collect(),
        &rev1,
        vec![],
    )
    .await?;
    catalog.publish(&m1).await?;

    let rev2 = vec![chunk("rev-b", "revision two content", vec![0.0, 1.0])];
    let outputs2 = build_shards(
        &rev2,
        &td.path().join("built-r2"),
        1,
        &BuildOptions {
            ivf_lists: 1,
            ..BuildOptions::default()
        },
    )?;
    let mut m2 = publish_fixture(
        store.clone(),
        outputs2.into_iter().map(|output| output.0).collect(),
        &rev2,
        vec![],
    )
    .await?;
    m2.revision = 2;
    m2.parent_revision = Some(1);
    m2.created_at_unix_ms = 2;
    catalog.publish(&m2).await?;

    let rt = CairnRuntime::new(store, td.path().join("cache"));

    let head = rt
        .chunks_by_id_prefix("t", "kb", &lookup_request("rev-"))
        .await?;
    assert_eq!(head.revision, 2);
    assert_eq!(
        head.hits
            .iter()
            .map(|hit| hit.id.as_str())
            .collect::<Vec<_>>(),
        vec!["rev-b"]
    );

    let mut historical = lookup_request("rev-");
    historical.revision = Some(1);
    let pinned = rt.chunks_by_id_prefix("t", "kb", &historical).await?;
    assert_eq!(pinned.revision, 1);
    assert_eq!(
        pinned
            .hits
            .iter()
            .map(|hit| hit.id.as_str())
            .collect::<Vec<_>>(),
        vec!["rev-a"]
    );
    Ok(())
}
