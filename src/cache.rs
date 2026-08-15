use crate::manifest::UqaBundleDescriptor;
use crate::object_store::{sha256_file, ObjectStore};
use anyhow::{bail, Context, Result};
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Mutex;

const ACCESS_TOUCH_INTERVAL_MS: u128 = 30_000;

#[derive(Clone)]
pub struct BundleCache {
    root: PathBuf,
    store: Arc<dyn ObjectStore>,
    locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
    active: Arc<DashMap<String, Arc<AtomicUsize>>>,
    verified: Arc<DashMap<String, ()>>,
    last_touch: Arc<DashMap<String, u128>>,
    max_bytes: Option<u64>,
}

pub struct BundleLease {
    path: PathBuf,
    counter: Arc<AtomicUsize>,
    downloaded_bytes: u64,
}

impl BundleLease {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn downloaded_bytes(&self) -> u64 {
        self.downloaded_bytes
    }
}

impl Drop for BundleLease {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

impl BundleCache {
    pub fn new(root: impl Into<PathBuf>, store: Arc<dyn ObjectStore>) -> Self {
        Self::new_with_limit(root, store, None)
    }

    pub fn new_with_limit(
        root: impl Into<PathBuf>,
        store: Arc<dyn ObjectStore>,
        max_bytes: Option<u64>,
    ) -> Self {
        Self {
            root: root.into(),
            store,
            locks: Arc::new(DashMap::new()),
            active: Arc::new(DashMap::new()),
            verified: Arc::new(DashMap::new()),
            last_touch: Arc::new(DashMap::new()),
            max_bytes,
        }
    }

    pub async fn lease_if_ready(&self, b: &UqaBundleDescriptor) -> Result<Option<BundleLease>> {
        validate_filename(&b.database_filename)?;
        crate::object_store::validate_object_key(&b.key)?;
        if b.sha256.len() != 64 || !b.sha256.bytes().all(|x| x.is_ascii_hexdigit()) {
            bail!("invalid UQA bundle SHA-256")
        }
        if b.size == 0 {
            bail!("UQA bundle has zero size")
        }
        let lock = self
            .locks
            .entry(b.sha256.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        let dir = self.root.join(&b.sha256);
        let final_path = dir.join(&b.database_filename);
        let ready = dir.join("READY");
        if tokio::fs::metadata(&ready).await.is_err()
            || tokio::fs::metadata(&final_path).await.is_err()
        {
            return Ok(None);
        }
        // Never hash-scan a multi-GB persisted bundle on the user request path.
        // A bundle is warm only after this process has verified it. On process
        // restart an existing READY entry is treated as a cold miss; the normal
        // background `activate()` path verifies/rebuilds it before later queries
        // can obtain a lease.
        if !self.verified.contains_key(&b.sha256) {
            return Ok(None);
        }
        self.touch_access_if_stale(&b.sha256, &dir).await?;
        let counter = self
            .active
            .entry(b.sha256.clone())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone();
        counter.fetch_add(1, Ordering::AcqRel);
        Ok(Some(BundleLease {
            path: final_path,
            counter,
            downloaded_bytes: 0,
        }))
    }

    pub async fn activate(&self, b: &UqaBundleDescriptor) -> Result<BundleLease> {
        validate_filename(&b.database_filename)?;
        crate::object_store::validate_object_key(&b.key)?;
        if b.sha256.len() != 64 || !b.sha256.bytes().all(|x| x.is_ascii_hexdigit()) {
            bail!("invalid UQA bundle SHA-256")
        }
        if b.size == 0 {
            bail!("UQA bundle has zero size")
        }
        let lock = self
            .locks
            .entry(b.sha256.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let guard = lock.lock().await;
        let dir = self.root.join(&b.sha256);
        let final_path = dir.join(&b.database_filename);
        let ready = dir.join("READY");
        tokio::fs::create_dir_all(&self.root).await?;
        let mut downloaded_bytes = 0u64;

        let ready_exists = tokio::fs::metadata(&ready).await.is_ok()
            && tokio::fs::metadata(&final_path).await.is_ok();
        let mut needs_download = !ready_exists;
        if ready_exists {
            // Persistent container disks can contain torn/corrupted cache state.
            // A verification failure is a cache miss, not a permanent outage:
            // quarantine the bad entry and rebuild it from the immutable object.
            if !self.verified.contains_key(&b.sha256) {
                match verify_existing(&dir, &final_path, b).await {
                    Ok(()) => {
                        self.verified.insert(b.sha256.clone(), ());
                    }
                    Err(error) => {
                        tracing::warn!(digest = %b.sha256, %error, "discarding corrupt UQA cache entry");
                        let corrupt = self.root.join(format!(
                            ".corrupt-{}-{}",
                            b.sha256,
                            uuid::Uuid::new_v4()
                        ));
                        if tokio::fs::rename(&dir, &corrupt).await.is_ok() {
                            let _ = tokio::fs::remove_dir_all(&corrupt).await;
                        } else {
                            let _ = tokio::fs::remove_dir_all(&dir).await;
                        }
                        self.verified.remove(&b.sha256);
                        needs_download = true;
                    }
                }
            }
            if !needs_download {
                self.touch_access_if_stale(&b.sha256, &dir).await?;
            }
        }
        if needs_download {
            // TempDir gives failure-atomic cleanup for every `?` path below. The
            // previous hand-written cleanup missed errors while hashing/writing
            // marker files and could strand multi-GB temporary directories.
            let temp = tempfile::Builder::new()
                .prefix(&format!(".{}-", b.sha256))
                .tempdir_in(&self.root)
                .context("create UQA activation temporary directory")?;
            let tmp = temp.path();
            let tmp_file = tmp.join(&b.database_filename);
            let downloaded = self.store.download_to(&b.key, &tmp_file, b.size).await?;
            downloaded_bytes = downloaded;
            if downloaded != b.size {
                bail!(
                    "UQA bundle size mismatch: downloaded {downloaded}, expected {}",
                    b.size
                )
            }
            if sha256_file(&tmp_file).await? != b.sha256 {
                bail!("UQA bundle checksum mismatch")
            }
            sync_file(&tmp_file).await?;
            tokio::fs::write(tmp.join("SIZE"), b.size.to_string()).await?;
            tokio::fs::write(tmp.join("READY"), b.sha256.as_bytes()).await?;
            let access_ms = unix_ms()?;
            write_access(tmp, access_ms).await?;
            sync_file(&tmp.join("SIZE")).await?;
            sync_file(&tmp.join("READY")).await?;
            sync_file(&tmp.join("ACCESS")).await?;

            // Keep transfers ownership of cleanup to us only after the fully
            // verified cache entry is complete.
            let persisted_tmp = temp.keep();
            if tokio::fs::metadata(&dir).await.is_ok() {
                let _ = tokio::fs::remove_dir_all(&dir).await;
            }
            if let Err(error) = tokio::fs::rename(&persisted_tmp, &dir).await {
                let _ = tokio::fs::remove_dir_all(&persisted_tmp).await;
                return Err(error).with_context(|| format!("activate {}", b.sha256));
            }
            sync_directory(&self.root).await?;
            self.verified.insert(b.sha256.clone(), ());
            self.last_touch.insert(b.sha256.clone(), access_ms);
        }

        let counter = self
            .active
            .entry(b.sha256.clone())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone();
        counter.fetch_add(1, Ordering::AcqRel);
        drop(guard);
        let lease = BundleLease {
            path: final_path,
            counter,
            downloaded_bytes,
        };
        self.evict_if_needed(Some(&b.sha256)).await?;
        Ok(lease)
    }

    async fn touch_access_if_stale(&self, digest: &str, dir: &Path) -> Result<()> {
        let now = unix_ms()?;
        let fresh = self
            .last_touch
            .get(digest)
            .is_some_and(|previous| now.saturating_sub(*previous) < ACCESS_TOUCH_INTERVAL_MS);
        if fresh {
            return Ok(());
        }
        write_access(dir, now).await?;
        self.last_touch.insert(digest.to_owned(), now);
        Ok(())
    }

    async fn evict_if_needed(&self, exclude: Option<&str>) -> Result<()> {
        let Some(max_bytes) = self.max_bytes else {
            return Ok(());
        };
        let mut entries = tokio::fs::read_dir(&self.root).await?;
        let mut candidates = Vec::new();
        let mut total = 0u64;
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if tokio::fs::metadata(path.join("READY")).await.is_err() {
                continue;
            }
            let size = read_u64(path.join("SIZE")).await.unwrap_or(0);
            let access = read_u128(path.join("ACCESS")).await.unwrap_or(0);
            total = total.saturating_add(size);
            candidates.push((access, name, path, size));
        }
        if total <= max_bytes {
            return Ok(());
        }
        candidates.sort_by_key(|x| x.0);
        for (_, digest, path, size) in candidates {
            if total <= max_bytes {
                break;
            }
            if exclude == Some(digest.as_str()) {
                continue;
            }
            let lock = self
                .locks
                .entry(digest.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone();
            let _guard = lock.lock().await;
            let active = self
                .active
                .get(&digest)
                .map(|x| x.load(Ordering::Acquire))
                .unwrap_or(0);
            if active != 0 {
                continue;
            }
            if tokio::fs::metadata(path.join("READY")).await.is_ok() {
                tokio::fs::remove_dir_all(&path).await?;
                sync_directory(&self.root).await?;
                self.verified.remove(&digest);
                self.active.remove(&digest);
                self.last_touch.remove(&digest);
                total = total.saturating_sub(size);
            }
        }
        Ok(())
    }
}

async fn verify_existing(dir: &Path, file: &Path, b: &UqaBundleDescriptor) -> Result<()> {
    let ready = String::from_utf8(tokio::fs::read(dir.join("READY")).await?)?;
    if ready != b.sha256 {
        bail!("warm cache READY marker mismatch")
    }
    let size = tokio::fs::metadata(file).await?.len();
    if size != b.size {
        bail!("warm cache file size mismatch")
    }
    if read_u64(dir.join("SIZE")).await? != b.size {
        bail!("warm cache SIZE marker mismatch")
    }
    if sha256_file(file).await? != b.sha256 {
        bail!("warm cache checksum mismatch")
    }
    Ok(())
}

fn validate_filename(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
    {
        bail!("unsafe database filename")
    }
    Ok(())
}

fn unix_ms() -> Result<u128> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis())
}

async fn write_access(dir: &Path, now: u128) -> Result<()> {
    tokio::fs::write(dir.join("ACCESS"), now.to_string()).await?;
    Ok(())
}

async fn read_u64(path: PathBuf) -> Result<u64> {
    Ok(String::from_utf8(tokio::fs::read(path).await?)?
        .trim()
        .parse()?)
}

async fn read_u128(path: PathBuf) -> Result<u128> {
    Ok(String::from_utf8(tokio::fs::read(path).await?)?
        .trim()
        .parse()?)
}

async fn sync_file(path: &Path) -> Result<()> {
    tokio::fs::File::open(path).await?.sync_all().await?;
    Ok(())
}
#[cfg(unix)]
async fn sync_directory(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::File::open(path)?.sync_all()
    })
    .await
    .context("join cache directory fsync task")??;
    Ok(())
}

#[cfg(not(unix))]
async fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}
