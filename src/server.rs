use crate::{
    manifest::{validate_scope_component, RevisionManifest},
    model::{ChunkLookupRequest, ChunkLookupResponse, SearchRequest, SearchResponse},
    CairnRuntime,
};
use anyhow::{bail, Result};
use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Path, State},
    http::{
        header::{AUTHORIZATION, WWW_AUTHENTICATE},
        HeaderMap, HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{net::SocketAddr, sync::Arc};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

pub const CAIRN_HTTP_API_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerScope {
    pub tenant: String,
    pub knowledge_base: String,
}

impl ServerScope {
    pub fn validate(&self) -> Result<()> {
        validate_scope_component("tenant", &self.tenant)?;
        validate_scope_component("knowledge_base", &self.knowledge_base)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub listen: SocketAddr,
    pub allow_historical_revisions: bool,
    pub max_request_bytes: usize,
    /// Optional bearer token. Health/version remain public; KB endpoints require
    /// this token when configured.
    pub bearer_token: Option<String>,
    /// Optional authorization fence for one tenant/knowledge-base pair. This is
    /// recommended for a DeepSeek Harness sidecar so a bearer token cannot be
    /// replayed against another namespace on the same CAIRN server.
    pub allowed_scope: Option<ServerScope>,
    /// Deliberate escape hatch for trusted private networks. A non-loopback bind
    /// without authentication is rejected unless this is explicitly enabled.
    pub allow_unauthenticated_non_loopback: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            allow_historical_revisions: false,
            max_request_bytes: 2 * 1024 * 1024,
            bearer_token: None,
            allowed_scope: None,
            allow_unauthenticated_non_loopback: false,
        }
    }
}

impl ServerOptions {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.max_request_bytes > 0 && self.max_request_bytes <= 64 * 1024 * 1024,
            "max_request_bytes must be in 1..=64MiB",
        );
        if let Some(token) = &self.bearer_token {
            let valid = (16..=4096).contains(&token.len())
                && token.bytes().all(|byte| byte.is_ascii_graphic());
            anyhow::ensure!(
                valid,
                "server bearer token must contain 16..=4096 visible ASCII bytes without whitespace",
            );
        }
        if let Some(scope) = &self.allowed_scope {
            scope.validate()?;
        }
        if !self.listen.ip().is_loopback()
            && self.bearer_token.is_none()
            && !self.allow_unauthenticated_non_loopback
        {
            bail!(
                "refusing unauthenticated non-loopback bind {}; set the configured server token environment variable or pass --allow-unauthenticated explicitly",
                self.listen,
            );
        }
        Ok(())
    }
}

#[derive(Clone)]
struct AppState {
    runtime: Arc<CairnRuntime>,
    allow_historical_revisions: bool,
    bearer_token_sha256: Option<[u8; 32]>,
    allowed_scope: Option<Arc<ServerScope>>,
}

#[derive(Debug, Serialize)]
struct ApiErrorEnvelope {
    error: ApiErrorBody,
}

#[derive(Debug, Serialize)]
struct ApiErrorBody {
    code: &'static str,
    message: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
}

struct ApiRejection {
    inner: Box<(StatusCode, HeaderMap, Json<ApiErrorEnvelope>)>,
}

impl ApiRejection {
    fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.inner.1
    }
}

impl IntoResponse for ApiRejection {
    fn into_response(self) -> Response {
        self.inner.into_response()
    }
}

#[derive(Debug, Serialize)]
pub struct KnowledgeBaseHeadResponse {
    pub tenant: String,
    pub knowledge_base: String,
    pub revision: u64,
    pub parent_revision: Option<u64>,
    pub created_at_unix_ms: u64,
    pub embedding_provider: String,
    pub embedding_model: String,
    pub dimension: u32,
    pub shard_count: usize,
    pub has_uqa_bundle: bool,
}

impl KnowledgeBaseHeadResponse {
    fn from_manifest(manifest: RevisionManifest) -> Self {
        Self {
            tenant: manifest.tenant,
            knowledge_base: manifest.knowledge_base,
            revision: manifest.revision,
            parent_revision: manifest.parent_revision,
            created_at_unix_ms: manifest.created_at_unix_ms,
            embedding_provider: manifest.embedding_provider,
            embedding_model: manifest.embedding_model,
            dimension: manifest.dimension,
            shard_count: manifest.shards.len(),
            has_uqa_bundle: manifest.uqa_bundle.is_some(),
        }
    }
}

fn app_router(state: AppState, max_request_bytes: usize) -> Router {
    Router::new()
        .route(
            "/health",
            get(|| async {
                Json(json!({
                    "ok": true,
                    "service": "cairn",
                    "version": env!("CARGO_PKG_VERSION"),
                    "api_version": CAIRN_HTTP_API_VERSION,
                }))
            }),
        )
        .route(
            "/version",
            get(|| async {
                Json(json!({
                    "service": "cairn",
                    "version": env!("CARGO_PKG_VERSION"),
                    "api_version": CAIRN_HTTP_API_VERSION,
                }))
            }),
        )
        .route("/v1/{tenant}/kb/{kb}/head", get(head))
        .route("/v1/{tenant}/kb/{kb}/search", post(search))
        .route(
            "/v1/{tenant}/kb/{kb}/chunks/by-id-prefix",
            post(chunks_by_id_prefix),
        )
        .with_state(state)
        .layer(DefaultBodyLimit::max(max_request_bytes))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
}

pub async fn serve(runtime: CairnRuntime, options: ServerOptions) -> Result<()> {
    options.validate()?;
    let ServerOptions {
        listen,
        allow_historical_revisions,
        max_request_bytes,
        bearer_token,
        allowed_scope,
        allow_unauthenticated_non_loopback: _,
    } = options;
    let authentication_enabled = bearer_token.is_some();
    let bearer_token_sha256 = bearer_token.as_deref().map(token_digest);
    drop(bearer_token);
    let state = AppState {
        runtime: Arc::new(runtime),
        allow_historical_revisions,
        bearer_token_sha256,
        allowed_scope: allowed_scope.map(Arc::new),
    };
    let router = app_router(state, max_request_bytes);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(
        listen = %listen,
        authentication = authentication_enabled,
        "CAIRN server started",
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn head(
    State(state): State<AppState>,
    Path((tenant, kb)): Path<(String, String)>,
    headers: HeaderMap,
) -> std::result::Result<Json<KnowledgeBaseHeadResponse>, ApiRejection> {
    let request_id = request_id(&headers);
    let tool_call_id = dsh_tool_call_id(&headers);
    authorize(&state, &headers, request_id.clone())?;
    tracing::debug!(
        tenant = %tenant,
        knowledge_base = %kb,
        request_id = request_id.as_deref().unwrap_or("-"),
        dsh_tool_call_id = tool_call_id.as_deref().unwrap_or("-"),
        "CAIRN head request admitted",
    );
    validate_scope_component("tenant", &tenant)
        .and_then(|_| validate_scope_component("knowledge_base", &kb))
        .map_err(|error| {
            api_error(
                StatusCode::BAD_REQUEST,
                "INVALID_SCOPE",
                error.to_string(),
                false,
                request_id.clone(),
            )
        })?;
    authorize_scope(&state, &tenant, &kb, request_id.clone())?;

    state
        .runtime
        .catalog
        .resolve(&tenant, &kb, None)
        .await
        .map(KnowledgeBaseHeadResponse::from_manifest)
        .map(Json)
        .map_err(|error| classify_runtime_error(error, request_id))
}

async fn search(
    State(state): State<AppState>,
    Path((tenant, kb)): Path<(String, String)>,
    headers: HeaderMap,
    payload: std::result::Result<Json<SearchRequest>, JsonRejection>,
) -> std::result::Result<Json<SearchResponse>, ApiRejection> {
    let request_id = request_id(&headers);
    let tool_call_id = dsh_tool_call_id(&headers);
    authorize(&state, &headers, request_id.clone())?;
    tracing::debug!(
        tenant = %tenant,
        knowledge_base = %kb,
        request_id = request_id.as_deref().unwrap_or("-"),
        dsh_tool_call_id = tool_call_id.as_deref().unwrap_or("-"),
        "CAIRN search request admitted",
    );
    let Json(request) = payload.map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "INVALID_JSON",
            error.body_text(),
            false,
            request_id.clone(),
        )
    })?;
    validate_scope_component("tenant", &tenant)
        .and_then(|_| validate_scope_component("knowledge_base", &kb))
        .and_then(|_| request.validate())
        .map_err(|error| {
            api_error(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                error.to_string(),
                false,
                request_id.clone(),
            )
        })?;
    authorize_scope(&state, &tenant, &kb, request_id.clone())?;
    if request.revision.is_some() && !state.allow_historical_revisions {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "HISTORICAL_REVISION_DISABLED",
            "historical revision search is disabled".to_owned(),
            false,
            request_id,
        ));
    }

    state
        .runtime
        .search(&tenant, &kb, &request)
        .await
        .map(Json)
        .map_err(|error| classify_runtime_error(error, request_id))
}

async fn chunks_by_id_prefix(
    State(state): State<AppState>,
    Path((tenant, kb)): Path<(String, String)>,
    headers: HeaderMap,
    payload: std::result::Result<Json<ChunkLookupRequest>, JsonRejection>,
) -> std::result::Result<Json<ChunkLookupResponse>, ApiRejection> {
    let request_id = request_id(&headers);
    let tool_call_id = dsh_tool_call_id(&headers);
    authorize(&state, &headers, request_id.clone())?;
    tracing::debug!(
        tenant = %tenant,
        knowledge_base = %kb,
        request_id = request_id.as_deref().unwrap_or("-"),
        dsh_tool_call_id = tool_call_id.as_deref().unwrap_or("-"),
        "CAIRN chunk id-prefix lookup request admitted",
    );
    let Json(request) = payload.map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "INVALID_JSON",
            error.body_text(),
            false,
            request_id.clone(),
        )
    })?;
    validate_scope_component("tenant", &tenant)
        .and_then(|_| validate_scope_component("knowledge_base", &kb))
        .and_then(|_| request.validate())
        .map_err(|error| {
            api_error(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                error.to_string(),
                false,
                request_id.clone(),
            )
        })?;
    authorize_scope(&state, &tenant, &kb, request_id.clone())?;
    if let Some(revision) = request.revision {
        if !state.allow_historical_revisions {
            // An explicit pin of the current HEAD is not a historical lookup:
            // the MCP bridge HEAD-pins every request, so that pin must stay
            // legal while any non-HEAD revision remains forbidden.
            let head_revision = state
                .runtime
                .catalog
                .get_head(&tenant, &kb)
                .await
                .map_err(|error| classify_runtime_error(error, request_id.clone()))?
                .map(|(head, _)| head.revision);
            if head_revision != Some(revision) {
                return Err(api_error(
                    StatusCode::FORBIDDEN,
                    "HISTORICAL_REVISION_DISABLED",
                    "historical revision lookup is disabled".to_owned(),
                    false,
                    request_id,
                ));
            }
        }
    }

    state
        .runtime
        .chunks_by_id_prefix(&tenant, &kb, &request)
        .await
        .map(Json)
        .map_err(|error| classify_runtime_error(error, request_id))
}

fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    request_id: Option<String>,
) -> std::result::Result<(), ApiRejection> {
    let Some(expected) = state.bearer_token_sha256.as_ref() else {
        return Ok(());
    };
    let candidate = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_bearer_token);
    if candidate.is_some_and(|candidate| secure_token_eq(expected, candidate)) {
        return Ok(());
    }
    let mut rejection = api_error(
        StatusCode::UNAUTHORIZED,
        "UNAUTHORIZED",
        "a valid bearer token is required".to_owned(),
        false,
        request_id,
    );
    rejection.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static(r#"Bearer realm="cairn""#),
    );
    Err(rejection)
}

fn parse_bearer_token(raw: &str) -> Option<&str> {
    let mut parts = raw.split_ascii_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    (scheme.eq_ignore_ascii_case("bearer") && parts.next().is_none()).then_some(token)
}

fn token_digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

fn secure_token_eq(expected_digest: &[u8; 32], candidate: &str) -> bool {
    let candidate_digest = token_digest(candidate);
    expected_digest
        .iter()
        .zip(candidate_digest.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn authorize_scope(
    state: &AppState,
    tenant: &str,
    knowledge_base: &str,
    request_id: Option<String>,
) -> std::result::Result<(), ApiRejection> {
    let Some(scope) = state.allowed_scope.as_deref() else {
        return Ok(());
    };
    if scope.tenant == tenant && scope.knowledge_base == knowledge_base {
        return Ok(());
    }
    Err(api_error(
        StatusCode::FORBIDDEN,
        "SCOPE_FORBIDDEN",
        "the authenticated CAIRN server instance is not authorized for this tenant/knowledge-base scope".to_owned(),
        false,
        request_id,
    ))
}

fn request_id(headers: &HeaderMap) -> Option<String> {
    safe_correlation_header(headers, "x-request-id")
}

fn dsh_tool_call_id(headers: &HeaderMap) -> Option<String> {
    safe_correlation_header(headers, "x-dsh-tool-call-id")
}

fn safe_correlation_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 256
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
                })
        })
        .map(str::to_owned)
}

fn api_error(
    status: StatusCode,
    code: &'static str,
    message: String,
    retryable: bool,
    request_id: Option<String>,
) -> ApiRejection {
    ApiRejection {
        inner: Box::new((
            status,
            HeaderMap::new(),
            Json(ApiErrorEnvelope {
                error: ApiErrorBody {
                    code,
                    message,
                    retryable,
                    request_id,
                },
            }),
        )),
    }
}

fn classify_runtime_error(error: anyhow::Error, request_id: Option<String>) -> ApiRejection {
    let message = error.to_string();
    let (status, code, public_message, retryable) = if message
        .contains("knowledge base has no HEAD")
        || message.contains("manifest not found")
    {
        (
            StatusCode::NOT_FOUND,
            "KNOWLEDGE_BASE_NOT_FOUND",
            "knowledge base or revision was not found".to_owned(),
            false,
        )
    } else if message.contains("no embedder is configured")
        || message.contains("OpenRouter API key")
    {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "EMBEDDING_UNAVAILABLE",
            "automatic query embedding is unavailable; configure the OpenRouter key or provide a query vector".to_owned(),
            false,
        )
    } else if message.contains("uses external embeddings")
        || message.contains("cannot derive a compatible query vector")
    {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "QUERY_VECTOR_REQUIRED",
            "this revision requires an explicit compatible query vector or an intentional lexical-only request".to_owned(),
            false,
        )
    } else if message.contains("exceeds runtime policy")
        || message.contains("dimension mismatch")
        || message.contains("unsupported revision embedding provider")
    {
        (
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            message.clone(),
            false,
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "SEARCH_FAILED",
            "search failed".to_owned(),
            true,
        )
    };
    tracing::warn!(
        error = %error,
        code,
        request_id = request_id.as_deref().unwrap_or("-"),
        "CAIRN request failed",
    );
    api_error(status, code, public_message, retryable, request_id)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::ServerOptions;
    use super::{
        app_router, parse_bearer_token, safe_correlation_header, secure_token_eq, token_digest,
        AppState, ServerScope,
    };
    use anyhow::Result;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use std::net::SocketAddr;
    use std::sync::Arc;

    async fn lookup_fixture_state() -> Result<(AppState, tempfile::TempDir)> {
        use crate::index::builder::{build_shards, revision_stats_from_chunks, BuildOptions};
        use crate::manifest::{
            upload_content_addressed, upload_revision_stats, Catalog, RevisionManifest,
        };
        use crate::model::{ChunkInput, GlobalScoringContext};
        use crate::object_store::{LocalStore, ObjectStore};
        use crate::CairnRuntime;
        use bytes::Bytes;
        use std::collections::BTreeMap;

        let td = tempfile::tempdir()?;
        let chunks = vec![
            ChunkInput {
                id: "srv-1".into(),
                text: "route fixture one".into(),
                vector: vec![1.0, 0.0],
                metadata: BTreeMap::new(),
            },
            ChunkInput {
                id: "srv-2".into(),
                text: "route fixture two".into(),
                vector: vec![0.0, 1.0],
                metadata: BTreeMap::new(),
            },
        ];
        let outputs = build_shards(
            &chunks,
            &td.path().join("built"),
            1,
            &BuildOptions {
                ivf_lists: 2,
                ..BuildOptions::default()
            },
        )?;
        let store: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(td.path().join("store")));
        let mut shards = Vec::new();
        for (path, _) in outputs {
            let bytes = Bytes::from(tokio::fs::read(path).await?);
            shards.push(
                upload_content_addressed(store.clone(), "objects/shards", bytes, "cairn").await?,
            );
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
            embedding_model: "test".into(),
            dimension: 2,
            shards,
            stats,
            tombstones: vec![],
            uqa_bundle: None,
        };
        Catalog::new(store.clone()).publish(&manifest).await?;
        let runtime = CairnRuntime::new(store, td.path().join("cache"));
        let state = AppState {
            runtime: Arc::new(runtime),
            allow_historical_revisions: false,
            bearer_token_sha256: None,
            allowed_scope: None,
        };
        Ok((state, td))
    }

    async fn spawn_app(state: AppState) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
        let router = app_router(state, 2 * 1024 * 1024);
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let addr = listener.local_addr()?;
        let handle = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::warn!(%error, "test server exited");
            }
        });
        Ok((addr, handle))
    }

    #[tokio::test]
    async fn by_id_prefix_route_validates_requests_and_looks_up() -> Result<()> {
        let (state, _fixture) = lookup_fixture_state().await?;
        let (addr, server) = spawn_app(state).await?;
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/v1/t/kb/kb/chunks/by-id-prefix");

        let unknown_field = client
            .post(&url)
            .json(&serde_json::json!({"id_prefix": "srv-", "bogus": 1}))
            .send()
            .await?;
        assert_eq!(unknown_field.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = unknown_field.json().await?;
        assert_eq!(body["error"]["code"], "INVALID_JSON");

        for invalid in [
            serde_json::json!({"id_prefix": ""}),
            serde_json::json!({"id_prefix": "srv-\n"}),
            serde_json::json!({"id_prefix": "srv-", "limit": 0}),
            serde_json::json!({"id_prefix": "srv-", "limit": 1001}),
            serde_json::json!({"id_prefix": "srv-", "max_range_reads": 4}),
            serde_json::json!({"id_prefix": "srv-", "max_remote_bytes": 10}),
        ] {
            let response = client.post(&url).json(&invalid).send().await?;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "invalid body {invalid} must be rejected"
            );
            let body: serde_json::Value = response.json().await?;
            assert_eq!(body["error"]["code"], "INVALID_REQUEST");
        }

        // Unknown order_by values must fail typed serde decoding, not request
        // validation.
        for invalid in [
            serde_json::json!({"id_prefix": "srv-", "order_by": "chronological"}),
            serde_json::json!({"id_prefix": "srv-", "order_by": 3}),
        ] {
            let response = client.post(&url).json(&invalid).send().await?;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "unknown order_by {invalid} must be rejected"
            );
            let body: serde_json::Value = response.json().await?;
            assert_eq!(body["error"]["code"], "INVALID_JSON");
        }

        // An explicit pin of the current HEAD revision is not a historical
        // lookup: the MCP bridge always HEAD-pins, so this must succeed even
        // with historical revisions disabled.
        let head_pinned = client
            .post(&url)
            .json(&serde_json::json!({"id_prefix": "srv-", "revision": 1}))
            .send()
            .await?;
        assert_eq!(head_pinned.status(), StatusCode::OK);
        let body: serde_json::Value = head_pinned.json().await?;
        assert_eq!(body["revision"], 1);

        let historical = client
            .post(&url)
            .json(&serde_json::json!({"id_prefix": "srv-", "revision": 2}))
            .send()
            .await?;
        assert_eq!(historical.status(), StatusCode::FORBIDDEN);
        let body: serde_json::Value = historical.json().await?;
        assert_eq!(body["error"]["code"], "HISTORICAL_REVISION_DISABLED");

        let ok = client
            .post(&url)
            .json(&serde_json::json!({"id_prefix": "srv-"}))
            .send()
            .await?;
        assert_eq!(ok.status(), StatusCode::OK);
        let decoded: crate::model::ChunkLookupResponse = ok.json().await?;
        assert_eq!(decoded.revision, 1);
        assert_eq!(decoded.embedding_provider, "external");
        assert!(
            decoded.corpus_sha256.len() == 64
                && decoded
                    .corpus_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "response must pin the revision corpus digest"
        );
        let ids: Vec<&str> = decoded.hits.iter().map(|hit| hit.id.as_str()).collect();
        assert_eq!(ids, vec!["srv-1", "srv-2"]);
        assert!(decoded.remote_bytes > 0);
        assert!(decoded.range_reads > 0);

        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn by_id_prefix_route_applies_auth_and_scope_rules() -> Result<()> {
        let (mut state, _fixture) = lookup_fixture_state().await?;
        let token = "0123456789abcdef0123456789abcdef";
        state.bearer_token_sha256 = Some(token_digest(token));
        state.allowed_scope = Some(Arc::new(ServerScope {
            tenant: "t".into(),
            knowledge_base: "kb".into(),
        }));
        let (addr, server) = spawn_app(state).await?;
        let client = reqwest::Client::new();
        let scoped = format!("http://{addr}/v1/t/kb/kb/chunks/by-id-prefix");
        let unscoped = format!("http://{addr}/v1/acme/kb/handbook/chunks/by-id-prefix");

        let anonymous = client
            .post(&scoped)
            .json(&serde_json::json!({"id_prefix": "srv-"}))
            .send()
            .await?;
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        let wrong_scope = client
            .post(&unscoped)
            .bearer_auth(token)
            .json(&serde_json::json!({"id_prefix": "srv-"}))
            .send()
            .await?;
        assert_eq!(wrong_scope.status(), StatusCode::FORBIDDEN);
        let body: serde_json::Value = wrong_scope.json().await?;
        assert_eq!(body["error"]["code"], "SCOPE_FORBIDDEN");

        let authorized = client
            .post(&scoped)
            .bearer_auth(token)
            .json(&serde_json::json!({"id_prefix": "srv-"}))
            .send()
            .await?;
        assert_eq!(authorized.status(), StatusCode::OK);

        server.abort();
        Ok(())
    }

    #[test]
    fn non_loopback_requires_auth_by_default() {
        let options = ServerOptions {
            listen: SocketAddr::from(([0, 0, 0, 0], 8080)),
            ..ServerOptions::default()
        };
        assert!(options.validate().is_err());
    }

    #[test]
    fn loopback_without_auth_is_allowed() -> Result<()> {
        ServerOptions::default().validate()
    }

    #[test]
    fn bearer_comparison_is_value_sensitive() {
        let digest = token_digest("0123456789abcdef");
        assert!(secure_token_eq(&digest, "0123456789abcdef"));
        assert!(!secure_token_eq(&digest, "0123456789abcdeg"));
        assert!(!secure_token_eq(&digest, "short"));
    }

    #[test]
    fn non_loopback_with_valid_auth_is_allowed() -> Result<()> {
        ServerOptions {
            listen: SocketAddr::from(([0, 0, 0, 0], 8080)),
            bearer_token: Some("0123456789abcdef0123456789abcdef".to_owned()),
            ..ServerOptions::default()
        }
        .validate()
    }

    #[test]
    fn server_token_rejects_whitespace_and_non_ascii() {
        for invalid in ["0123456789abcde ", "0123456789abcde한", "short"] {
            let options = ServerOptions {
                bearer_token: Some(invalid.to_owned()),
                ..ServerOptions::default()
            };
            assert!(options.validate().is_err());
        }
    }

    #[test]
    fn bearer_parser_accepts_exactly_one_scheme_and_token() {
        assert_eq!(parse_bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(parse_bearer_token("bearer abc"), Some("abc"));
        assert_eq!(parse_bearer_token("Bearer abc extra"), None);
        assert_eq!(parse_bearer_token("Basic abc"), None);
    }

    #[test]
    fn correlation_headers_are_bounded_and_safe() -> Result<()> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-dsh-tool-call-id",
            HeaderValue::from_static("call_42:child-1"),
        );
        assert_eq!(
            safe_correlation_header(&headers, "x-dsh-tool-call-id").as_deref(),
            Some("call_42:child-1"),
        );
        headers.insert(
            "x-dsh-tool-call-id",
            HeaderValue::from_static("contains space"),
        );
        assert!(safe_correlation_header(&headers, "x-dsh-tool-call-id").is_none());
        Ok(())
    }

    #[test]
    fn configured_scope_is_validated() {
        let options = ServerOptions {
            allowed_scope: Some(ServerScope {
                tenant: "acme".to_owned(),
                knowledge_base: "handbook".to_owned(),
            }),
            ..ServerOptions::default()
        };
        assert!(options.validate().is_ok());

        let invalid = ServerOptions {
            allowed_scope: Some(ServerScope {
                tenant: "../escape".to_owned(),
                knowledge_base: "handbook".to_owned(),
            }),
            ..ServerOptions::default()
        };
        assert!(invalid.validate().is_err());
    }
}
