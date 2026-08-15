use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use futures::StreamExt;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_MATCH, IF_NONE_MATCH, RANGE};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub size: u64,
    pub etag: Option<String>,
}

#[async_trait]
pub trait ObjectStore: Send + Sync {
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>>;
    async fn get_range(&self, key: &str, offset: u64, len: u64) -> Result<Bytes>;
    async fn put(
        &self,
        key: &str,
        bytes: Bytes,
        condition: PutCondition,
    ) -> Result<Option<ObjectMeta>>;
    async fn delete(&self, key: &str) -> Result<()>;
    async fn download_to(&self, key: &str, path: &Path, max_bytes: u64) -> Result<u64>;
    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        condition: PutCondition,
    ) -> Result<Option<ObjectMeta>>;
}

#[derive(Debug, Clone, Default)]
pub enum PutCondition {
    #[default]
    Any,
    CreateOnly,
    MatchEtag(String),
}

#[derive(Debug, Clone)]
pub struct LocalStore {
    root: PathBuf,
    locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
}

impl LocalStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            locks: Arc::new(DashMap::new()),
        }
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        validate_object_key(key)?;
        Ok(self.root.join(key))
    }

    async fn meta_path(&self, p: &Path) -> Result<Option<ObjectMeta>> {
        match tokio::fs::metadata(p).await {
            Ok(m) if m.is_file() => Ok(Some(ObjectMeta {
                size: m.len(),
                etag: Some(sha256_file(p).await?),
            })),
            Ok(_) => bail!("object path is not a regular file: {}", p.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn key_lock(&self, key: &str) -> Arc<Mutex<()>> {
        self.locks
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn cross_process_lock(&self, object_path: &Path) -> Result<std::fs::File> {
        let filename = object_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid object filename")?;
        let lock_path = object_path.with_file_name(format!(".{filename}.lock"));
        let parent = lock_path
            .parent()
            .context("object lock has no parent")?
            .to_path_buf();
        tokio::fs::create_dir_all(&parent).await?;
        tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)?;
            fs2::FileExt::lock_exclusive(&file)?;
            Ok(file)
        })
        .await
        .context("join local-store file-lock task")?
    }

    async fn put_impl(
        &self,
        key: &str,
        source: PutSource<'_>,
        condition: PutCondition,
    ) -> Result<Option<ObjectMeta>> {
        let p = self.path(key)?;
        let lock = self.key_lock(key);
        let _guard = lock.lock().await;
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let _process_guard = self.cross_process_lock(&p).await?;
        let current = self.meta_path(&p).await?;
        if !condition_matches(&condition, current.as_ref()) {
            return Ok(None);
        }
        let file_name = p
            .file_name()
            .and_then(|x| x.to_str())
            .context("invalid object filename")?;
        let tmp = p.with_file_name(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()));
        let write_result: Result<()> = async {
            match source {
                PutSource::Bytes(bytes) => tokio::fs::write(&tmp, bytes).await?,
                PutSource::File(path) => {
                    tokio::fs::copy(path, &tmp).await?;
                }
            }
            tokio::fs::File::open(&tmp).await?.sync_all().await?;
            Ok(())
        }
        .await;
        if let Err(error) = write_result {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error);
        }
        if let Err(error) = tokio::fs::rename(&tmp, &p).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error.into());
        }
        if let Some(parent) = p.parent() {
            sync_directory(parent).await?;
        }
        self.meta_path(&p).await
    }
}

enum PutSource<'a> {
    Bytes(&'a Bytes),
    File(&'a Path),
}

fn condition_matches(condition: &PutCondition, current: Option<&ObjectMeta>) -> bool {
    match condition {
        PutCondition::Any => true,
        PutCondition::CreateOnly => current.is_none(),
        PutCondition::MatchEtag(expected) => {
            current.and_then(|m| m.etag.as_deref()) == Some(expected.as_str())
        }
    }
}

#[async_trait]
impl ObjectStore for LocalStore {
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        self.meta_path(&self.path(key)?).await
    }

    async fn get_range(&self, key: &str, offset: u64, len: u64) -> Result<Bytes> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        if len == 0 {
            return Ok(Bytes::new());
        }
        let mut f = tokio::fs::File::open(self.path(key)?).await?;
        let size = f.metadata().await?.len();
        let end = offset.checked_add(len).context("range overflow")?;
        if offset > size || end > size {
            bail!("invalid range {offset}+{len} for object of {size} bytes")
        }
        f.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut buf = vec![0; usize::try_from(len).context("range too large")?];
        f.read_exact(&mut buf).await?;
        Ok(Bytes::from(buf))
    }

    async fn put(
        &self,
        key: &str,
        bytes: Bytes,
        condition: PutCondition,
    ) -> Result<Option<ObjectMeta>> {
        self.put_impl(key, PutSource::Bytes(&bytes), condition)
            .await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let lock = self.key_lock(key);
        let _guard = lock.lock().await;
        let path = self.path(key)?;
        let _process_guard = self.cross_process_lock(&path).await?;
        match tokio::fs::remove_file(&path).await {
            Ok(_) => {
                if let Some(parent) = path.parent() {
                    sync_directory(parent).await?;
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn download_to(&self, key: &str, path: &Path, max_bytes: u64) -> Result<u64> {
        let src = self.path(key)?;
        let size = tokio::fs::metadata(&src).await?.len();
        if size > max_bytes {
            bail!("object exceeds download limit: {size} > {max_bytes}")
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await?;
        let tmp = temporary_sibling(path)?;
        let result: Result<u64> = async {
            let copied = tokio::fs::copy(&src, &tmp).await?;
            if copied != size {
                bail!("local object copy size changed: {copied} != {size}")
            }
            tokio::fs::File::open(&tmp).await?.sync_all().await?;
            tokio::fs::rename(&tmp, path).await?;
            sync_directory(parent).await?;
            Ok(copied)
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        result
    }

    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        condition: PutCondition,
    ) -> Result<Option<ObjectMeta>> {
        self.put_impl(key, PutSource::File(path), condition).await
    }
}

#[derive(Debug, Clone)]
pub struct HttpStore {
    base: String,
    bearer: Option<String>,
    client: reqwest::Client,
}

impl HttpStore {
    pub fn new(base: impl Into<String>, bearer: Option<String>) -> Result<Self> {
        let raw = base.into();
        let parsed = reqwest::Url::parse(&raw).context("invalid object-gateway URL")?;
        let host = parsed
            .host_str()
            .context("object-gateway URL must include a host")?;
        let local_http =
            parsed.scheme() == "http" && matches!(host, "localhost" | "127.0.0.1" | "::1");
        if parsed.scheme() != "https" && !local_http {
            bail!("object gateway must use HTTPS; plain HTTP is allowed only for localhost")
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            bail!("object-gateway URL must not contain credentials; use a bearer-token environment variable")
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            bail!("object-gateway URL must not contain a query string or fragment")
        }
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build HTTP object-store client")?;
        Ok(Self {
            base: raw.trim_end_matches('/').to_string(),
            bearer,
            client,
        })
    }

    fn url(&self, key: &str) -> Result<String> {
        validate_object_key(key)?;
        Ok(format!(
            "{}/objects/{}",
            self.base,
            key.split('/').map(url_escape).collect::<Vec<_>>().join("/")
        ))
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(t) = &self.bearer {
            req.bearer_auth(t)
        } else {
            req
        }
    }

    fn condition(
        &self,
        mut req: reqwest::RequestBuilder,
        condition: &PutCondition,
    ) -> reqwest::RequestBuilder {
        match condition {
            PutCondition::Any => {}
            PutCondition::CreateOnly => req = req.header(IF_NONE_MATCH, "*"),
            PutCondition::MatchEtag(x) => {
                req = req.header(IF_MATCH, format!("\"{}\"", trim_etag(x)))
            }
        }
        req
    }

    fn put_result(&self, r: &reqwest::Response, size: u64) -> Result<Option<ObjectMeta>> {
        if r.status() == reqwest::StatusCode::PRECONDITION_FAILED
            || r.status() == reqwest::StatusCode::CONFLICT
        {
            return Ok(None);
        }
        if !r.status().is_success() {
            bail!("PUT failed: {}", r.status())
        }
        Ok(Some(ObjectMeta {
            size,
            etag: r
                .headers()
                .get(ETAG)
                .and_then(|x| x.to_str().ok())
                .map(trim_etag),
        }))
    }
}

#[async_trait]
impl ObjectStore for HttpStore {
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        let r = self
            .auth(
                self.client
                    .head(self.url(key)?)
                    .timeout(std::time::Duration::from_secs(30)),
            )
            .send()
            .await?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !r.status().is_success() {
            bail!("HEAD failed: {}", r.status())
        }
        let size = r
            .headers()
            .get(CONTENT_LENGTH)
            .context("HEAD response missing Content-Length")?
            .to_str()?
            .parse::<u64>()?;
        Ok(Some(ObjectMeta {
            size,
            etag: r
                .headers()
                .get(ETAG)
                .and_then(|x| x.to_str().ok())
                .map(trim_etag),
        }))
    }

    async fn get_range(&self, key: &str, offset: u64, len: u64) -> Result<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let end = offset.checked_add(len - 1).context("range overflow")?;
        let r = self
            .auth(
                self.client
                    .get(self.url(key)?)
                    .header(RANGE, format!("bytes={offset}-{end}"))
                    .timeout(std::time::Duration::from_secs(30)),
            )
            .send()
            .await?;
        if r.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            bail!("range GET expected 206, got {}", r.status())
        }
        let expected_range = format!("bytes {offset}-{end}/");
        let content_range = r
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|x| x.to_str().ok())
            .context("range response missing Content-Range")?;
        let total = content_range
            .strip_prefix(&expected_range)
            .with_context(|| format!("unexpected Content-Range: {content_range}"))?
            .parse::<u64>()
            .context("invalid Content-Range object size")?;
        if total <= end {
            bail!("Content-Range total {total} does not contain requested end {end}")
        }
        let bytes = r.bytes().await?;
        if bytes.len() != usize::try_from(len).context("range too large")? {
            bail!(
                "range server returned {} bytes, expected {len}",
                bytes.len()
            )
        }
        Ok(bytes)
    }

    async fn put(
        &self,
        key: &str,
        bytes: Bytes,
        condition: PutCondition,
    ) -> Result<Option<ObjectMeta>> {
        let size = u64::try_from(bytes.len()).context("PUT body length exceeds u64")?;
        let req = self
            .condition(self.client.put(self.url(key)?).body(bytes), &condition)
            .timeout(std::time::Duration::from_secs(2 * 60));
        let r = self.auth(req).send().await?;
        self.put_result(&r, size)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let r = self
            .auth(
                self.client
                    .delete(self.url(key)?)
                    .timeout(std::time::Duration::from_secs(30)),
            )
            .send()
            .await?;
        if !r.status().is_success() && r.status() != reqwest::StatusCode::NOT_FOUND {
            bail!("DELETE failed: {}", r.status())
        }
        Ok(())
    }

    async fn download_to(&self, key: &str, path: &Path, max_bytes: u64) -> Result<u64> {
        use tokio::io::AsyncWriteExt;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await?;
        let tmp = temporary_sibling(path)?;
        let result: Result<u64> = async {
            let r = self
                .auth(
                    self.client
                        .get(self.url(key)?)
                        .timeout(std::time::Duration::from_secs(30 * 60)),
                )
                .send()
                .await?;
            if !r.status().is_success() {
                bail!("GET failed: {}", r.status())
            }
            if let Some(length) = r.content_length() {
                if length > max_bytes {
                    bail!("object exceeds download limit: {length} > {max_bytes}")
                }
            }
            let mut f = tokio::fs::File::create(&tmp).await?;
            let mut total = 0u64;
            let mut stream = r.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                total = total
                    .checked_add(
                        u64::try_from(chunk.len()).context("download chunk length overflow")?,
                    )
                    .context("download length overflow")?;
                if total > max_bytes {
                    bail!("object exceeds download limit while streaming: {total} > {max_bytes}")
                }
                f.write_all(&chunk).await?;
            }
            f.flush().await?;
            f.sync_all().await?;
            drop(f);
            tokio::fs::rename(&tmp, path).await?;
            sync_directory(parent).await?;
            Ok(total)
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        result
    }

    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        condition: PutCondition,
    ) -> Result<Option<ObjectMeta>> {
        let size = tokio::fs::metadata(path).await?.len();
        let file = tokio::fs::File::open(path).await?;
        let stream = tokio_util::io::ReaderStream::new(file);
        let body = reqwest::Body::wrap_stream(stream);
        let req = self.condition(
            self.client
                .put(self.url(key)?)
                .header(CONTENT_LENGTH, size)
                .body(body),
            &condition,
        );
        let r = self
            .auth(req.timeout(std::time::Duration::from_secs(30 * 60)))
            .send()
            .await?;
        self.put_result(&r, size)
    }
}

fn temporary_sibling(path: &Path) -> Result<PathBuf> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("download destination has no valid filename")?;
    Ok(path.with_file_name(format!(".{filename}.tmp-{}", uuid::Uuid::new_v4())))
}

#[cfg(unix)]
async fn sync_directory(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::File::open(path)?.sync_all()
    })
    .await
    .context("join directory fsync task")??;
    Ok(())
}

#[cfg(not(unix))]
async fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn validate_object_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 4096 || key.starts_with('/') || key.ends_with('/') {
        bail!("unsafe object key")
    }
    for segment in key.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.contains('\\')
            || segment.contains('\0')
        {
            bail!("unsafe object key")
        }
    }
    Ok(())
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

pub async fn sha256_file(path: &Path) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path).await?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let digest = h.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    Ok(out)
}

fn trim_etag(x: &str) -> String {
    x.trim_matches('"').to_string()
}

fn url_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char)
        } else {
            use std::fmt::Write as _;
            let _ = write!(&mut out, "%{b:02X}");
        }
    }
    out
}
