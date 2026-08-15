use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::{stream, StreamExt, TryStreamExt};
use reqwest::{header::RETRY_AFTER, Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

pub const DEFAULT_OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_OPENROUTER_MODEL: &str = "qwen/qwen3-embedding-8b";
pub const DEFAULT_OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";
pub const DEFAULT_EMBEDDING_DIMENSIONS: u32 = 1024;
pub const DEFAULT_EMBEDDING_BATCH_SIZE: usize = 32;
pub const DEFAULT_EMBEDDING_MAX_BATCH_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_EMBEDDING_CONCURRENCY: usize = 4;
pub const DEFAULT_EMBEDDING_TIMEOUT_SECS: u64 = 60;
const MAX_EMBEDDING_BATCH_SIZE: usize = 256;
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
const MAX_SUCCESS_BODY_BYTES: usize = 128 * 1024 * 1024;
const MAX_RETRIES: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingSettings {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_dimensions")]
    pub dimensions: u32,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_max_batch_bytes")]
    pub max_batch_bytes: usize,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for EmbeddingSettings {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: default_model(),
            dimensions: default_dimensions(),
            base_url: default_base_url(),
            api_key_env: default_api_key_env(),
            batch_size: default_batch_size(),
            max_batch_bytes: default_max_batch_bytes(),
            concurrency: default_concurrency(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

impl EmbeddingSettings {
    pub fn validate(&self) -> Result<()> {
        if self.provider != "openrouter" {
            bail!("unsupported embedding provider '{}'; CAIRN currently supports 'openrouter'", self.provider)
        }
        if self.model.trim().is_empty() || self.model.len() > 512 {
            bail!("embedding.model must be 1..=512 bytes")
        }
        if self.dimensions == 0 || self.dimensions > 65_536 {
            bail!("embedding.dimensions must be in 1..=65536")
        }
        validate_https_or_localhost(&self.base_url)?;
        validate_env_name(&self.api_key_env)?;
        if self.batch_size == 0 || self.batch_size > MAX_EMBEDDING_BATCH_SIZE {
            bail!("embedding.batch_size must be in 1..={MAX_EMBEDDING_BATCH_SIZE}")
        }
        if self.max_batch_bytes == 0 || self.max_batch_bytes > 64 * 1024 * 1024 {
            bail!("embedding.max_batch_bytes must be in 1..=64MiB")
        }
        if self.concurrency == 0 || self.concurrency > 32 {
            bail!("embedding.concurrency must be in 1..=32")
        }
        if self.timeout_secs == 0 || self.timeout_secs > 600 {
            bail!("embedding.timeout_secs must be in 1..=600")
        }
        Ok(())
    }

    pub fn api_key(&self) -> Result<String> {
        self.validate()?;
        std::env::var(&self.api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .with_context(|| {
                format!(
                    "{} is not set; export an OpenRouter API key or provide vectors explicitly",
                    self.api_key_env
                )
            })
    }

    pub fn client(&self) -> Result<Arc<dyn TextEmbedder>> {
        let api_key = self.api_key()?;
        Ok(Arc::new(OpenRouterEmbedder::new(self.clone(), api_key)?))
    }

    /// Build an embedder only when the API key is present. This lets long-lived
    /// servers start in lexical/vector-explicit mode and surface a request-local
    /// error only when automatic OpenRouter embedding is actually needed.
    pub fn client_if_configured(&self) -> Result<Option<Arc<dyn TextEmbedder>>> {
        self.validate()?;
        let Some(api_key) = std::env::var(&self.api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        Ok(Some(Arc::new(OpenRouterEmbedder::new(self.clone(), api_key)?)))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum EmbeddingInputType {
    Query,
    Document,
}

impl EmbeddingInputType {
    const fn as_openrouter_str(self) -> &'static str {
        match self {
            Self::Query => "search_query",
            Self::Document => "search_document",
        }
    }
}

#[async_trait]
pub trait TextEmbedder: Send + Sync {
    async fn embed(
        &self,
        model: &str,
        dimensions: u32,
        input_type: EmbeddingInputType,
        inputs: &[&str],
    ) -> Result<Vec<Vec<f32>>>;
}

#[derive(Clone)]
pub struct OpenRouterEmbedder {
    settings: EmbeddingSettings,
    api_key: Arc<str>,
    client: Client,
}

impl OpenRouterEmbedder {
    pub fn new(settings: EmbeddingSettings, api_key: String) -> Result<Self> {
        settings.validate()?;
        if api_key.trim().is_empty() {
            bail!("OpenRouter API key must not be empty")
        }
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(settings.timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build OpenRouter HTTP client")?;
        Ok(Self { settings, api_key: Arc::from(api_key), client })
    }

    async fn embed_batch(
        &self,
        model: &str,
        dimensions: u32,
        input_type: EmbeddingInputType,
        inputs: &[&str],
    ) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new())
        }
        if inputs.iter().any(|input| input.trim().is_empty()) {
            bail!("OpenRouter embedding inputs must not be empty")
        }
        let url = format!("{}/embeddings", self.settings.base_url.trim_end_matches('/'));
        let payload = EmbeddingRequest {
            model,
            dimensions,
            encoding_format: "float",
            input_type: input_type.as_openrouter_str(),
            input: inputs,
        };

        for attempt in 0..=MAX_RETRIES {
            let response = self.client
                .post(&url)
                .bearer_auth(self.api_key.as_ref())
                .json(&payload)
                .send()
                .await;

            let response = match response {
                Ok(response) => response,
                Err(error) if attempt < MAX_RETRIES && is_retryable_transport_error(&error) => {
                    tokio::time::sleep(retry_delay(attempt + 1, None)).await;
                    continue;
                },
                Err(error) => return Err(error).context("send OpenRouter embeddings request"),
            };

            let status = response.status();
            if status.is_success() {
                let body = bounded_body(response, MAX_SUCCESS_BODY_BYTES, "OpenRouter embeddings response").await?;
                let body: EmbeddingResponse = serde_json::from_slice(&body)
                    .context("decode OpenRouter embeddings response")?;
                return validate_embedding_response(body, model, inputs.len(), dimensions);
            }

            let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_secs);
            let message = bounded_error_body(response).await;
            if !retryable || attempt >= MAX_RETRIES {
                bail!("OpenRouter embeddings request failed with HTTP {status}: {message}")
            }
            tokio::time::sleep(retry_delay(attempt + 1, retry_after)).await;
        }
        bail!("OpenRouter embeddings retry loop exhausted unexpectedly")
    }

}

#[async_trait]
impl TextEmbedder for OpenRouterEmbedder {
    async fn embed(
        &self,
        model: &str,
        dimensions: u32,
        input_type: EmbeddingInputType,
        inputs: &[&str],
    ) -> Result<Vec<Vec<f32>>> {
        if model.trim().is_empty() || model.len() > 512 {
            bail!("embedding model must be 1..=512 bytes")
        }
        if dimensions == 0 || dimensions > 65_536 {
            bail!("embedding dimensions must be in 1..=65536")
        }
        let mut ranges = Vec::new();
        let mut start = 0usize;
        while start < inputs.len() {
            let mut end = start;
            let mut bytes = 0usize;
            while end < inputs.len() && end.saturating_sub(start) < self.settings.batch_size {
                let next = inputs[end].len();
                if next > self.settings.max_batch_bytes {
                    bail!("embedding input {} is {} bytes, above embedding.max_batch_bytes={}", end, next, self.settings.max_batch_bytes)
                }
                let Some(total) = bytes.checked_add(next) else { bail!("embedding batch byte-size overflow") };
                if end > start && total > self.settings.max_batch_bytes { break }
                bytes = total;
                end = end.saturating_add(1);
            }
            if end == start { bail!("failed to construct a non-empty embedding batch") }
            ranges.push((start, end));
            start = end;
        }

        let batches = stream::iter(ranges.into_iter().map(|(start, end)| async move {
            self.embed_batch(model, dimensions, input_type, &inputs[start..end]).await
        }))
        .buffered(self.settings.concurrency)
        .try_collect::<Vec<_>>()
        .await?;

        Ok(batches.into_iter().flatten().collect())
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    dimensions: u32,
    encoding_format: &'static str,
    input_type: &'static str,
    input: &'a [&'a str],
}

#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
    model: String,
}

#[derive(Debug, Deserialize)]
struct EmbeddingDatum {
    embedding: Vec<f32>,
    index: usize,
}

fn validate_embedding_response(
    mut response: EmbeddingResponse,
    expected_model: &str,
    expected: usize,
    dimensions: u32,
) -> Result<Vec<Vec<f32>>> {
    if response.model != expected_model {
        bail!("OpenRouter returned model '{}', expected '{expected_model}'", response.model)
    }
    if response.data.len() != expected {
        bail!("OpenRouter returned {} embeddings for {expected} inputs", response.data.len())
    }
    response.data.sort_by_key(|datum| datum.index);
    let expected_dimensions = usize::try_from(dimensions).context("embedding dimension does not fit usize")?;
    let mut output = Vec::with_capacity(expected);
    for (position, datum) in response.data.into_iter().enumerate() {
        if datum.index != position {
            bail!("OpenRouter embedding response has missing/duplicate index at position {position}")
        }
        if datum.embedding.len() != expected_dimensions {
            bail!(
                "OpenRouter embedding dimension mismatch: expected {expected_dimensions}, got {}",
                datum.embedding.len()
            )
        }
        if datum.embedding.iter().any(|value| !value.is_finite()) {
            bail!("OpenRouter returned a non-finite embedding value")
        }
        let norm_sq = datum.embedding.iter().map(|value| value * value).sum::<f32>();
        if !norm_sq.is_finite() || norm_sq <= 1e-24 {
            bail!("OpenRouter returned a zero/invalid embedding")
        }
        output.push(datum.embedding);
    }
    Ok(output)
}

async fn bounded_body(response: reqwest::Response, max_bytes: usize, label: &str) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        if length > max_bytes as u64 {
            bail!("{label} exceeds {max_bytes} bytes")
        }
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(next) = stream.next().await {
        let chunk = next.with_context(|| format!("read {label}"))?;
        let new_len = body.len().checked_add(chunk.len()).context("HTTP response size overflow")?;
        if new_len > max_bytes {
            bail!("{label} exceeds {max_bytes} bytes")
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn bounded_error_body(response: reqwest::Response) -> String {
    match bounded_body(response, MAX_ERROR_BODY_BYTES, "OpenRouter error response").await {
        Ok(body) => String::from_utf8_lossy(&body).replace('\r', " ").replace('\n', " "),
        Err(_) => "<unavailable or oversized error body>".to_owned(),
    }
}

fn retry_delay(attempt: usize, retry_after: Option<Duration>) -> Duration {
    let exponential = Duration::from_millis(250u64.saturating_mul(1u64 << attempt.min(4)));
    retry_after.unwrap_or(exponential).min(Duration::from_secs(30))
}

fn is_retryable_transport_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect()
}

fn validate_https_or_localhost(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).context("embedding.base_url is not a valid URL")?;
    let host = parsed.host_str().context("embedding.base_url must include a host")?;
    let local_http = parsed.scheme() == "http" && matches!(host, "localhost" | "127.0.0.1" | "::1");
    if parsed.scheme() != "https" && !local_http {
        bail!("embedding.base_url must use https:// (plain http is allowed only for localhost)")
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("embedding.base_url must not contain credentials")
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("embedding.base_url must not contain a query string or fragment")
    }
    Ok(())
}

fn validate_env_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && name.as_bytes().first().is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_');
    if !valid {
        bail!("invalid embedding API-key environment variable name: {name}")
    }
    Ok(())
}

fn default_provider() -> String { "openrouter".to_owned() }
fn default_model() -> String { DEFAULT_OPENROUTER_MODEL.to_owned() }
fn default_dimensions() -> u32 { DEFAULT_EMBEDDING_DIMENSIONS }
fn default_base_url() -> String { DEFAULT_OPENROUTER_BASE_URL.to_owned() }
fn default_api_key_env() -> String { DEFAULT_OPENROUTER_API_KEY_ENV.to_owned() }
fn default_batch_size() -> usize { DEFAULT_EMBEDDING_BATCH_SIZE }
fn default_max_batch_bytes() -> usize { DEFAULT_EMBEDDING_MAX_BATCH_BYTES }
fn default_concurrency() -> usize { DEFAULT_EMBEDDING_CONCURRENCY }
fn default_timeout_secs() -> u64 { DEFAULT_EMBEDDING_TIMEOUT_SECS }

#[cfg(test)]
mod tests {
    use super::{validate_embedding_response, EmbeddingDatum, EmbeddingResponse, EmbeddingSettings, DEFAULT_OPENROUTER_MODEL};

    #[test]
    fn defaults_to_openrouter_qwen3_embedding() -> anyhow::Result<()> {
        let settings = EmbeddingSettings::default();
        settings.validate()?;
        assert_eq!(settings.provider, "openrouter");
        assert_eq!(settings.model, DEFAULT_OPENROUTER_MODEL);
        assert_eq!(settings.dimensions, 1024);
        Ok(())
    }

    #[test]
    fn response_indices_are_restored_to_input_order() -> anyhow::Result<()> {
        let response = EmbeddingResponse {
            model: "test/model".to_owned(),
            data: vec![
                EmbeddingDatum { embedding: vec![0.0, 1.0], index: 1 },
                EmbeddingDatum { embedding: vec![1.0, 0.0], index: 0 },
            ],
        };
        let output = validate_embedding_response(response, "test/model", 2, 2)?;
        assert_eq!(output, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        Ok(())
    }

    #[test]
    fn response_rejects_dimension_mismatch() {
        let response = EmbeddingResponse {
            model: "test/model".to_owned(),
            data: vec![EmbeddingDatum { embedding: vec![1.0], index: 0 }],
        };
        assert!(validate_embedding_response(response, "test/model", 1, 2).is_err());
    }

    #[test]
    fn response_rejects_unexpected_model() {
        let response = EmbeddingResponse {
            model: "other/model".to_owned(),
            data: vec![EmbeddingDatum { embedding: vec![1.0, 0.0], index: 0 }],
        };
        assert!(validate_embedding_response(response, "test/model", 1, 2).is_err());
    }

    #[test]
    fn rejects_plain_http_to_lookalike_localhost() {
        let mut settings = EmbeddingSettings::default();
        settings.base_url = "http://localhost.evil.example/api/v1".to_owned();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn accepts_plain_http_for_exact_localhost() -> anyhow::Result<()> {
        let mut settings = EmbeddingSettings::default();
        settings.base_url = "http://localhost:8080/api/v1".to_owned();
        settings.validate()
    }
}
