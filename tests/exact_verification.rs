//! Exact-string verification leg.
//!
//! Hybrid BM25+vector retrieval discovers candidates by intent, but tokenized
//! lexical scoring cannot assert that a chunk literally contains an identifier
//! such as `hydratePreferences(`. `require_text` closes the
//! discovery -> narrowing -> verification loop inside a single request instead
//! of forcing a caller to re-read every hit to check.

use bytes::Bytes;
use cairn_uqa::index::builder::{build_shards, revision_stats_from_chunks, BuildOptions};
use cairn_uqa::manifest::{
    upload_content_addressed, upload_revision_stats, Catalog, RevisionManifest,
};
use cairn_uqa::model::{ChunkInput, GlobalScoringContext, SearchRequest};
use cairn_uqa::object_store::{LocalStore, ObjectStore};
use cairn_uqa::CairnRuntime;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

fn chunk(id: &str, text: &str, vector: Vec<f32>) -> ChunkInput {
    ChunkInput {
        id: id.into(),
        text: text.into(),
        vector,
        metadata: BTreeMap::new(),
    }
}

/// `hydrate` and `reset` are lexically indistinguishable for the natural
/// language intent "restore theme preferences"; only one carries the
/// identifier a caller ultimately needs.
fn corpus() -> Vec<ChunkInput> {
    vec![
        chunk(
            "hydrate",
            "restore theme preferences by calling hydratePreferences on startup",
            vec![1.0, 0.0],
        ),
        chunk(
            "reset",
            "restore theme preferences to their packaged defaults",
            vec![0.9, 0.1],
        ),
        chunk(
            "korean",
            "세션 검색은 문서에서 관련 정보를 찾습니다",
            vec![0.0, 1.0],
        ),
    ]
}

fn request(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        query_vector: Vec::new(),
        limit: 10,
        candidate_limit: 32,
        revision: None,
        filters: BTreeMap::new(),
        require_text: Vec::new(),
        require_text_case_sensitive: false,
        max_text_bytes: 0,
        max_remote_bytes: 256 * 1024 * 1024,
        max_range_reads: 4096,
    }
}

async fn publish_fixture(store: Arc<dyn ObjectStore>, dir: &Path) -> anyhow::Result<()> {
    let all = corpus();
    let outputs = build_shards(
        &all,
        &dir.join("built"),
        1,
        &BuildOptions {
            ivf_lists: 2,
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
        &all,
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
        embedding_model: "offline-test".into(),
        dimension: 2,
        shards,
        stats,
        tombstones: vec![],
        uqa_bundle: None,
    };
    Catalog::new(store.clone()).publish(&manifest).await?;
    Ok(())
}

async fn runtime(td: &tempfile::TempDir) -> anyhow::Result<CairnRuntime> {
    let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
    publish_fixture(store.clone(), td.path()).await?;
    let mut rt = CairnRuntime::new(store, td.path().join("cache"));
    rt.auto_embed = false;
    Ok(rt)
}

fn ids(response: &cairn_uqa::model::SearchResponse) -> Vec<&str> {
    response.hits.iter().map(|hit| hit.id.as_str()).collect()
}

#[tokio::test]
async fn require_text_narrows_discovery_to_literal_matches() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let rt = runtime(&td).await?;

    // Discovery alone cannot separate the two competing chunks.
    let broad = rt
        .search("t", "kb", &request("restore theme preferences"))
        .await?;
    assert!(
        ids(&broad).contains(&"hydrate") && ids(&broad).contains(&"reset"),
        "fixture precondition: both chunks match the intent, got {:?}",
        ids(&broad)
    );
    assert!(
        !broad.approximate,
        "unfiltered lexical retrieval is exact for this fixture"
    );

    let mut req = request("restore theme preferences");
    req.require_text = vec!["hydratePreferences".to_owned()];
    let verified = rt.search("t", "kb", &req).await?;

    assert_eq!(
        ids(&verified),
        vec!["hydrate"],
        "require_text must drop candidates lacking the literal"
    );
    assert!(
        verified.approximate,
        "post-retrieval verification over a bounded candidate pool is approximate"
    );
    Ok(())
}

#[tokio::test]
async fn require_text_is_case_insensitive_unless_opted_in() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let rt = runtime(&td).await?;

    let mut insensitive = request("restore theme preferences");
    insensitive.require_text = vec!["HYDRATEpreferences".to_owned()];
    assert_eq!(
        ids(&rt.search("t", "kb", &insensitive).await?),
        vec!["hydrate"],
        "default matching ignores case"
    );

    let mut sensitive = insensitive.clone();
    sensitive.require_text_case_sensitive = true;
    assert!(
        rt.search("t", "kb", &sensitive).await?.hits.is_empty(),
        "case-sensitive matching must reject a mis-cased literal"
    );

    let mut exact_case = sensitive.clone();
    exact_case.require_text = vec!["hydratePreferences".to_owned()];
    assert_eq!(
        ids(&rt.search("t", "kb", &exact_case).await?),
        vec!["hydrate"],
        "case-sensitive matching accepts the exact literal"
    );
    Ok(())
}

#[tokio::test]
async fn require_text_matches_multibyte_literals_and_ands_needles() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let rt = runtime(&td).await?;

    // A multi-byte literal spanning a whitespace boundary is exactly what
    // tokenized BM25 cannot assert on its own.
    let mut cjk = request("검색");
    cjk.require_text = vec!["관련 정보".to_owned()];
    assert_eq!(
        ids(&rt.search("t", "kb", &cjk).await?),
        vec!["korean"],
        "multi-byte literal must match without splitting a codepoint"
    );

    let mut both = request("restore theme preferences");
    both.require_text = vec!["restore".to_owned(), "hydratePreferences".to_owned()];
    assert_eq!(
        ids(&rt.search("t", "kb", &both).await?),
        vec!["hydrate"],
        "every needle must be present, not any"
    );

    let mut unsatisfiable = request("restore theme preferences");
    unsatisfiable.require_text = vec!["hydratePreferences".to_owned(), "packaged".to_owned()];
    assert!(
        rt.search("t", "kb", &unsatisfiable).await?.hits.is_empty(),
        "needles spread across different chunks must match nothing"
    );
    Ok(())
}

#[test]
fn require_text_validation_rejects_malformed_needles() {
    let mut empty = request("q");
    empty.require_text = vec![String::new()];
    assert!(
        empty.validate().is_err(),
        "an empty needle matches everything and must be rejected"
    );

    let mut too_many = request("q");
    too_many.require_text = (0..17).map(|index| format!("needle{index}")).collect();
    assert!(
        too_many.validate().is_err(),
        "unbounded needle counts must be rejected"
    );

    let mut oversized = request("q");
    oversized.require_text = vec!["x".repeat(4097)];
    assert!(
        oversized.validate().is_err(),
        "oversized needles must be rejected"
    );

    let mut valid = request("q");
    valid.require_text = vec!["hydratePreferences".to_owned(), "관련 정보".to_owned()];
    assert!(
        valid.validate().is_ok(),
        "well-formed needles must validate"
    );
}
