//! Response-side context budget.
//!
//! A ranked list is normally consumed to decide *what to read*. Returning every
//! full chunk spends caller context on text that is never used, so
//! `max_text_bytes` ships a bounded preview and flags it, leaving the full
//! chunk addressable by id through the point-read path.

use bytes::Bytes;
use cairn_uqa::index::builder::{build_shards, revision_stats_from_chunks, BuildOptions};
use cairn_uqa::manifest::{
    upload_content_addressed, upload_revision_stats, Catalog, RevisionManifest,
};
use cairn_uqa::model::{
    ChunkInput, ChunkLookupOrder, ChunkLookupRequest, GlobalScoringContext, SearchRequest,
};
use cairn_uqa::object_store::{LocalStore, ObjectStore};
use cairn_uqa::CairnRuntime;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// Every Hangul syllable here is three bytes, so a byte budget lands
/// mid-codepoint unless truncation is boundary-aware. `hydratePreferences`
/// sits deliberately past any small budget.
const LONG_KO: &str =
    "세션 검색은 문서에서 관련 정보를 찾습니다 그리고 마지막에 hydratePreferences 를 호출합니다";

fn chunk(id: &str, text: &str, vector: Vec<f32>) -> ChunkInput {
    ChunkInput {
        id: id.into(),
        text: text.into(),
        vector,
        metadata: BTreeMap::new(),
    }
}

fn corpus() -> Vec<ChunkInput> {
    vec![
        chunk("ko-long", LONG_KO, vec![1.0, 0.0]),
        chunk("en-short", "short", vec![0.0, 1.0]),
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

#[tokio::test]
async fn unbounded_requests_return_full_untruncated_text() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let rt = runtime(&td).await?;

    let response = rt.search("t", "kb", &request("검색")).await?;
    let hit = response
        .hits
        .iter()
        .find(|hit| hit.id == "ko-long")
        .ok_or_else(|| anyhow::anyhow!("fixture precondition: ko-long must match"))?;

    assert_eq!(hit.text, LONG_KO, "default requests keep full text");
    assert!(!hit.text_truncated, "untruncated text must not be flagged");
    Ok(())
}

#[tokio::test]
async fn text_budget_truncates_on_a_character_boundary_and_flags_the_hit() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let rt = runtime(&td).await?;

    // 32 lands inside a 3-byte syllable, so a naive byte cut would panic or
    // emit invalid UTF-8. The query matches both chunks so the same response
    // carries an over-budget and an under-budget hit.
    let mut req = request("검색 short");
    req.max_text_bytes = 32;
    let response = rt.search("t", "kb", &req).await?;
    let hit = response
        .hits
        .iter()
        .find(|hit| hit.id == "ko-long")
        .ok_or_else(|| anyhow::anyhow!("fixture precondition: ko-long must match"))?;

    assert!(
        hit.text.len() <= 32,
        "preview must respect the budget, got {} bytes",
        hit.text.len()
    );
    assert!(hit.text_truncated, "a shortened preview must be flagged");
    assert!(
        LONG_KO.starts_with(hit.text.as_str()),
        "preview must be a prefix of the stored chunk, got {:?}",
        hit.text
    );
    assert!(
        !hit.text.is_empty(),
        "a 32-byte budget must still carry usable context"
    );

    // Chunks already inside the budget are returned whole and unflagged.
    let short = response
        .hits
        .iter()
        .find(|hit| hit.id == "en-short")
        .ok_or_else(|| anyhow::anyhow!("fixture precondition: en-short must match"))?;
    assert_eq!(short.text, "short");
    assert!(!short.text_truncated);

    // The bounded preview is not a dead end: the point-read path still
    // resolves the untruncated chunk by id.
    let full = rt
        .chunks_by_id_prefix(
            "t",
            "kb",
            &ChunkLookupRequest {
                id_prefix: "ko-long".into(),
                limit: 10,
                revision: None,
                order_by: ChunkLookupOrder::Id,
                max_remote_bytes: 256 * 1024 * 1024,
                max_range_reads: 4096,
            },
        )
        .await?;
    assert_eq!(
        full.hits.first().map(|hit| hit.text.as_str()),
        Some(LONG_KO),
        "chunk lookup must still serve the full text"
    );
    Ok(())
}

#[tokio::test]
async fn verification_runs_against_full_text_not_the_preview() -> anyhow::Result<()> {
    let td = tempfile::tempdir()?;
    let rt = runtime(&td).await?;

    // The needle sits far past the 32-byte budget. If truncation were applied
    // before verification, this would silently return nothing.
    let mut req = request("검색");
    req.max_text_bytes = 32;
    req.require_text = vec!["hydratePreferences".to_owned()];
    let response = rt.search("t", "kb", &req).await?;

    assert_eq!(
        response
            .hits
            .iter()
            .map(|hit| hit.id.as_str())
            .collect::<Vec<_>>(),
        vec!["ko-long"],
        "a needle beyond the preview budget must still match"
    );
    let hit = response
        .hits
        .first()
        .ok_or_else(|| anyhow::anyhow!("expected the verified hit"))?;
    assert!(hit.text_truncated, "the returned preview is still bounded");
    assert!(
        !hit.text.contains("hydratePreferences"),
        "precondition: the needle is outside the preview window"
    );
    Ok(())
}
