use crate::model::RevisionStats;
use crate::object_store::{hex_sha256, ObjectStore, PutCondition};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Read;
use std::sync::Arc;

const MAX_STATS_COMPRESSED: u64 = 128 * 1024 * 1024;
const MAX_STATS_RAW: u64 = 512 * 1024 * 1024;
const MAX_TOMBSTONE_COMPRESSED: u64 = 256 * 1024 * 1024;
const MAX_TOMBSTONE_RAW: u64 = 256 * 1024 * 1024;
const MAX_TOMBSTONE_RAW_TOTAL: u64 = 256 * 1024 * 1024;
const MAX_TOMBSTONE_IDS_TOTAL: u64 = 2_000_000;
const MAX_HEAD_BYTES: u64 = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_COMMIT_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardDescriptor {
    pub key: String,
    pub sha256: String,
    pub size: u64,
    pub format_version: u32,
    /// SHA-256 of the serialized block directory. Cold readers compare this
    /// against the range-read header before trusting any block offsets/hashes.
    pub directory_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionStatsDescriptor {
    pub key: String,
    /// SHA-256 of compressed object bytes.
    pub sha256: String,
    pub size: u64,
    /// SHA-256 of canonical uncompressed JSON bytes.
    pub raw_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TombstoneDescriptor {
    pub key: String,
    pub sha256: String,
    pub size: u64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UqaBundleDescriptor {
    pub materialized_revision: u64,
    pub key: String,
    pub sha256: String,
    pub size: u64,
    pub database_filename: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionManifest {
    pub tenant: String,
    pub knowledge_base: String,
    pub revision: u64,
    pub parent_revision: Option<u64>,
    pub created_at_unix_ms: u64,
    #[serde(default = "default_embedding_provider")]
    pub embedding_provider: String,
    pub embedding_model: String,
    pub dimension: u32,
    pub shards: Vec<ShardDescriptor>,
    pub stats: RevisionStatsDescriptor,
    #[serde(default)]
    pub tombstones: Vec<TombstoneDescriptor>,
    pub uqa_bundle: Option<UqaBundleDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Head {
    pub revision: u64,
    pub manifest_key: String,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitMarker {
    pub tenant: String,
    pub knowledge_base: String,
    pub revision: u64,
    pub manifest_key: String,
    pub manifest_sha256: String,
}

#[derive(Clone)]
pub struct Catalog {
    store: Arc<dyn ObjectStore>,
}

impl Catalog {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    pub fn head_key(tenant: &str, kb: &str) -> Result<String> {
        validate_scope_component("tenant", tenant)?;
        validate_scope_component("knowledge_base", kb)?;
        Ok(format!("tenants/{tenant}/kb/{kb}/HEAD.json"))
    }

    pub fn manifest_key(tenant: &str, kb: &str, revision: u64) -> Result<String> {
        validate_scope_component("tenant", tenant)?;
        validate_scope_component("knowledge_base", kb)?;
        if revision == 0 {
            bail!("revision must be positive")
        }
        Ok(format!(
            "tenants/{tenant}/kb/{kb}/revisions/{revision}.json"
        ))
    }

    pub fn commit_key(tenant: &str, kb: &str, revision: u64) -> Result<String> {
        validate_scope_component("tenant", tenant)?;
        validate_scope_component("knowledge_base", kb)?;
        if revision == 0 {
            bail!("revision must be positive")
        }
        Ok(format!("tenants/{tenant}/kb/{kb}/commits/{revision}.json"))
    }

    pub async fn get_head(&self, tenant: &str, kb: &str) -> Result<Option<(Head, Option<String>)>> {
        let key = Self::head_key(tenant, kb)?;
        let Some(meta) = self.store.head(&key).await? else {
            return Ok(None);
        };
        if meta.size == 0 || meta.size > MAX_HEAD_BYTES {
            bail!("invalid HEAD object size")
        }
        let bytes = self.store.get_range(&key, 0, meta.size).await?;
        let head: Head = serde_json::from_slice(&bytes)?;
        if head.manifest_key != Self::manifest_key(tenant, kb, head.revision)? {
            bail!("HEAD manifest key mismatch")
        }
        validate_digest(&head.manifest_sha256)?;
        Ok(Some((head, meta.etag)))
    }

    async fn read_manifest(
        &self,
        tenant: &str,
        kb: &str,
        revision: u64,
    ) -> Result<(RevisionManifest, String)> {
        let key = Self::manifest_key(tenant, kb, revision)?;
        let meta = self
            .store
            .head(&key)
            .await?
            .with_context(|| format!("manifest not found: {key}"))?;
        if meta.size == 0 || meta.size > MAX_MANIFEST_BYTES {
            bail!("invalid manifest size for {key}")
        }
        let bytes = self
            .store
            .get_range(&key, 0, meta.size)
            .await
            .with_context(|| format!("read manifest {key}"))?;
        let sha256 = hex_sha256(&bytes);
        let manifest: RevisionManifest = serde_json::from_slice(&bytes)?;
        validate_manifest_shape(&manifest)?;
        if manifest.tenant != tenant
            || manifest.knowledge_base != kb
            || manifest.revision != revision
        {
            bail!("manifest scope mismatch")
        }
        Ok((manifest, sha256))
    }

    async fn get_manifest_verified(
        &self,
        tenant: &str,
        kb: &str,
        revision: u64,
        expected_sha256: &str,
    ) -> Result<RevisionManifest> {
        validate_digest(expected_sha256)?;
        let (manifest, actual_sha256) = self.read_manifest(tenant, kb, revision).await?;
        if actual_sha256 != expected_sha256 {
            bail!("manifest SHA-256 mismatch for revision {revision}")
        }
        Ok(manifest)
    }

    async fn get_commit_marker(
        &self,
        tenant: &str,
        kb: &str,
        revision: u64,
    ) -> Result<CommitMarker> {
        let key = Self::commit_key(tenant, kb, revision)?;
        let meta = self
            .store
            .head(&key)
            .await?
            .with_context(|| format!("revision {revision} is not committed"))?;
        if meta.size == 0 || meta.size > MAX_COMMIT_BYTES {
            bail!("invalid commit marker size")
        }
        let bytes = self.store.get_range(&key, 0, meta.size).await?;
        let marker: CommitMarker = serde_json::from_slice(&bytes)?;
        if marker.tenant != tenant
            || marker.knowledge_base != kb
            || marker.revision != revision
            || marker.manifest_key != Self::manifest_key(tenant, kb, revision)?
        {
            bail!("commit marker scope mismatch")
        }
        validate_digest(&marker.manifest_sha256)?;
        Ok(marker)
    }

    pub async fn resolve(
        &self,
        tenant: &str,
        kb: &str,
        revision: Option<u64>,
    ) -> Result<RevisionManifest> {
        let (head, _) = self
            .get_head(tenant, kb)
            .await?
            .context("knowledge base has no HEAD")?;
        match revision {
            None => {
                self.get_manifest_verified(tenant, kb, head.revision, &head.manifest_sha256)
                    .await
            }
            Some(target) if target == head.revision => {
                self.get_manifest_verified(tenant, kb, target, &head.manifest_sha256)
                    .await
            }
            Some(target) => {
                let marker = self.get_commit_marker(tenant, kb, target).await?;
                self.get_manifest_verified(tenant, kb, target, &marker.manifest_sha256)
                    .await
            }
        }
    }

    /// Return the first unused immutable revision number after `after`.
    ///
    /// HEAD can legitimately move backwards through `promote`, while immutable
    /// manifests from later committed revisions remain in object storage. A
    /// writer must therefore never assume that `HEAD + 1` is unused.
    pub async fn next_available_revision(
        &self,
        tenant: &str,
        kb: &str,
        after: Option<u64>,
    ) -> Result<u64> {
        const MAX_PROBES: usize = 100_000;
        let mut candidate = match after {
            Some(revision) => revision.checked_add(1).context("revision overflow")?,
            None => 1,
        };
        for _ in 0..MAX_PROBES {
            let manifest_key = Self::manifest_key(tenant, kb, candidate)?;
            let commit_key = Self::commit_key(tenant, kb, candidate)?;
            let manifest_exists = self.store.head(&manifest_key).await?.is_some();
            let commit_exists = self.store.head(&commit_key).await?.is_some();
            if !manifest_exists && !commit_exists {
                return Ok(candidate);
            }
            candidate = candidate.checked_add(1).context("revision overflow")?;
        }
        bail!(
            "could not find an unused revision after {} within {MAX_PROBES} probes; pass --revision explicitly",
            after.map_or_else(|| "root".to_owned(), |revision| revision.to_string()),
        )
    }

    async fn ensure_commit_marker(
        &self,
        manifest: &RevisionManifest,
        manifest_sha256: &str,
    ) -> Result<()> {
        validate_digest(manifest_sha256)?;
        let marker = CommitMarker {
            tenant: manifest.tenant.clone(),
            knowledge_base: manifest.knowledge_base.clone(),
            revision: manifest.revision,
            manifest_key: Self::manifest_key(
                &manifest.tenant,
                &manifest.knowledge_base,
                manifest.revision,
            )?,
            manifest_sha256: manifest_sha256.to_owned(),
        };
        let bytes = Bytes::from(serde_json::to_vec(&marker)?);
        if bytes.is_empty() || bytes.len() as u64 > MAX_COMMIT_BYTES {
            bail!("commit marker exceeds size limit")
        }
        let key = Self::commit_key(
            &manifest.tenant,
            &manifest.knowledge_base,
            manifest.revision,
        )?;
        match self
            .store
            .put(&key, bytes.clone(), PutCondition::CreateOnly)
            .await?
        {
            Some(_) => Ok(()),
            None => {
                let meta = self
                    .store
                    .head(&key)
                    .await?
                    .context("existing commit marker disappeared")?;
                if meta.size == 0 || meta.size > MAX_COMMIT_BYTES {
                    bail!("existing commit marker has invalid size")
                }
                let existing = self.store.get_range(&key, 0, meta.size).await?;
                if existing != bytes {
                    bail!(
                        "revision {} has a conflicting commit marker",
                        manifest.revision
                    )
                }
                Ok(())
            }
        }
    }

    /// Publish a new revision, inferring shard inheritance from content-
    /// addressed key overlap with the parent. Any reused parent shard key
    /// pins the embedding provider/model/dimension to the parent's identity.
    pub async fn publish(&self, manifest: &RevisionManifest) -> Result<()> {
        self.publish_inner(manifest, false).await
    }

    /// Publish an explicit full shard replacement (`cairn snapshot` or
    /// `publish --replace-shards`). The caller asserts that every shard was
    /// rebuilt for this revision, so embedding provider/model/dimension may
    /// change even when a deterministically rebuilt shard is byte-identical to
    /// a parent shard and therefore keeps its content-addressed key. Ordinary
    /// or delta publication must use [`Catalog::publish`], which stays
    /// conservative.
    pub async fn publish_full_replacement(&self, manifest: &RevisionManifest) -> Result<()> {
        self.publish_inner(manifest, true).await
    }

    async fn publish_inner(
        &self,
        manifest: &RevisionManifest,
        full_shard_replacement: bool,
    ) -> Result<()> {
        validate_manifest_shape(manifest)?;
        let head_key = Self::head_key(&manifest.tenant, &manifest.knowledge_base)?;
        let current = self
            .get_head(&manifest.tenant, &manifest.knowledge_base)
            .await?;

        // Safe retry after a publish that already advanced HEAD. The manifest is
        // immutable; an identical retry is success, while a same-revision payload
        // mismatch is a hard conflict.
        if let Some((head, _)) = &current {
            if head.revision == manifest.revision {
                let (existing, manifest_sha256) = self
                    .read_manifest(
                        &manifest.tenant,
                        &manifest.knowledge_base,
                        manifest.revision,
                    )
                    .await?;
                if serde_json::to_vec(&existing)? == serde_json::to_vec(manifest)? {
                    if head.manifest_sha256 != manifest_sha256 {
                        bail!("HEAD manifest digest does not match stored manifest")
                    }
                    self.ensure_commit_marker(manifest, &manifest_sha256)
                        .await?;
                    return Ok(());
                }
                bail!(
                    "revision {} is already HEAD with different manifest content",
                    manifest.revision
                )
            }
        }

        match (&current, manifest.parent_revision) {
            (None, None) => {}
            (Some((h, _)), Some(parent)) if h.revision == parent => {
                if manifest.revision <= parent {
                    bail!("new revision {} must be greater than parent {parent}; use promote for rollback", manifest.revision)
                }
                let parent_manifest = self
                    .get_manifest_verified(
                        &manifest.tenant,
                        &manifest.knowledge_base,
                        parent,
                        &h.manifest_sha256,
                    )
                    .await?;
                let inherited = manifest
                    .shards
                    .iter()
                    .any(|s| parent_manifest.shards.iter().any(|p| p.key == s.key));
                if !full_shard_replacement
                    && inherited
                    && (manifest.dimension != parent_manifest.dimension
                        || manifest.embedding_model != parent_manifest.embedding_model
                        || manifest.embedding_provider != parent_manifest.embedding_provider)
                {
                    bail!("cannot change embedding model/dimension while inheriting parent shards; publish a full shard replacement")
                }
            }
            (None, Some(_)) => bail!("parent revision specified for new KB"),
            (Some((h, _)), None) => {
                bail!("parent revision required; current HEAD is {}", h.revision)
            }
            (Some((h, _)), Some(parent)) => {
                bail!("stale parent {parent}; current HEAD is {}", h.revision)
            }
        }

        // Validate expensive immutable dependencies only after cheap lineage/CAS
        // preconditions have passed. This keeps stale writers from spending R2
        // reads validating a revision that can never become HEAD.
        self.verify_references(manifest).await?;

        let key = Self::manifest_key(
            &manifest.tenant,
            &manifest.knowledge_base,
            manifest.revision,
        )?;
        let bytes = Bytes::from(serde_json::to_vec_pretty(manifest)?);
        if bytes.is_empty() || bytes.len() as u64 > MAX_MANIFEST_BYTES {
            bail!("serialized manifest exceeds size limit")
        }
        let manifest_sha256 = hex_sha256(&bytes);
        match self
            .store
            .put(&key, bytes.clone(), PutCondition::CreateOnly)
            .await?
        {
            Some(_) => {}
            None => {
                // An attacker or operator may have pre-created this immutable key.
                // Never use an unbounded GET on that object just to compare it.
                let meta = self
                    .store
                    .head(&key)
                    .await?
                    .context("existing manifest disappeared during conflict check")?;
                if meta.size == 0 || meta.size > MAX_MANIFEST_BYTES {
                    bail!("existing manifest object has invalid size")
                }
                let existing = self.store.get_range(&key, 0, meta.size).await?;
                if existing != bytes {
                    bail!(
                        "revision {} already exists with different content",
                        manifest.revision
                    )
                }
            }
        }

        // Before replacing an existing HEAD, durably record that currently-visible
        // revision in the immutable commit ledger. A process can crash after a
        // successful HEAD CAS but before writing the new revision's marker; repairing
        // the old HEAD here guarantees it never becomes an unreachable historical
        // revision when the next publish succeeds.
        if let Some((current_head, _)) = &current {
            let current_manifest = self
                .get_manifest_verified(
                    &manifest.tenant,
                    &manifest.knowledge_base,
                    current_head.revision,
                    &current_head.manifest_sha256,
                )
                .await?;
            self.ensure_commit_marker(&current_manifest, &current_head.manifest_sha256)
                .await?;
        }

        let head = Head {
            revision: manifest.revision,
            manifest_key: key,
            manifest_sha256: manifest_sha256.clone(),
        };
        let body = Bytes::from(serde_json::to_vec(&head)?);
        let cond = match current {
            None => PutCondition::CreateOnly,
            Some((_, etag)) => {
                PutCondition::MatchEtag(etag.context("store did not return HEAD etag")?)
            }
        };
        if self.store.put(&head_key, body, cond).await?.is_none() {
            bail!("concurrent HEAD update; immutable revision remains uncommitted")
        }
        // The current HEAD is authoritative even if a crash happens before this
        // marker write. The next publish/promote repairs the marker before moving
        // HEAD away, making the transition crash-recoverable without a multi-object
        // transaction.
        self.ensure_commit_marker(manifest, &manifest_sha256)
            .await?;
        Ok(())
    }

    async fn verify_references(&self, manifest: &RevisionManifest) -> Result<()> {
        // Publication is the trust boundary: HEAD must never point at a manifest
        // whose immutable dependencies are absent or structurally inconsistent.
        ensure_content_addressed_name(&manifest.stats.key, &manifest.stats.sha256)?;
        let (revision_stats, _, _) = read_revision_stats(self.store.clone(), &manifest.stats)
            .await
            .with_context(|| format!("invalid revision stats {}", manifest.stats.key))?;

        for shard in &manifest.shards {
            ensure_content_addressed_name(&shard.key, &shard.sha256)?;
            let meta = self
                .store
                .head(&shard.key)
                .await?
                .with_context(|| format!("missing shard {}", shard.key))?;
            if meta.size != shard.size {
                bail!("shard size mismatch for {}", shard.key)
            }
            let hb = self
                .store
                .get_range(&shard.key, 0, crate::binary::HEADER_SIZE as u64)
                .await?;
            let header = crate::binary::Header::decode(&hb)?;
            if header.version != shard.format_version
                || header.directory_sha256_hex() != shard.directory_sha256
            {
                bail!("shard header/manifest mismatch for {}", shard.key)
            }
            let dir_end = header
                .dir_offset
                .checked_add(header.dir_len)
                .context("shard directory range overflow")?;
            if dir_end != shard.size {
                bail!("shard directory does not terminate object {}", shard.key)
            }
            let db = self
                .store
                .get_range(&shard.key, header.dir_offset, header.dir_len)
                .await?;
            let dir = tokio::task::spawn_blocking(move || {
                crate::binary::decode_directory(header, db.as_ref())
            })
            .await
            .context("join shard-directory decode task")??;
            dir.validate_layout(shard.size, header.dir_offset, header.dir_len)?;

            // Validate the serving schema now, not on the first user query.
            let meta_ref = dir.get("meta", "main")?.clone();
            let compressed = self
                .store
                .get_range(&shard.key, meta_ref.offset, meta_ref.len)
                .await?;
            let meta_ref_for_decode = meta_ref.clone();
            let shard_meta: crate::index::ShardMeta = tokio::task::spawn_blocking(move || {
                crate::binary::decode_block(
                    &meta_ref_for_decode,
                    compressed.as_ref(),
                    crate::binary::MAX_BLOCK_RAW,
                )
            })
            .await
            .context("join shard-metadata decode task")??;
            shard_meta.validate()?;
            crate::index::validate_directory_schema(&dir, &shard_meta)?;
            if shard_meta.dimension != manifest.dimension {
                bail!(
                    "shard dimension {} does not match manifest dimension {} for {}",
                    shard_meta.dimension,
                    manifest.dimension,
                    shard.key
                )
            }
            if shard_meta.analyzer != revision_stats.analyzer {
                bail!(
                    "shard analyzer {} does not match revision stats analyzer {} for {}",
                    shard_meta.analyzer,
                    revision_stats.analyzer,
                    shard.key
                )
            }
        }

        for tombstone in &manifest.tombstones {
            ensure_content_addressed_name(&tombstone.key, &tombstone.sha256)?;
        }
        if !manifest.tombstones.is_empty() {
            let _ = read_tombstones(self.store.clone(), &manifest.tombstones).await?;
        }

        if let Some(bundle) = &manifest.uqa_bundle {
            ensure_content_addressed_name(&bundle.key, &bundle.sha256)?;
            let meta = self
                .store
                .head(&bundle.key)
                .await?
                .with_context(|| format!("missing UQA bundle {}", bundle.key))?;
            if meta.size != bundle.size {
                bail!("UQA bundle size mismatch for {}", bundle.key)
            }
        }
        Ok(())
    }

    /// Verify that a revision is committed, hash-bound, and has all serving
    /// dependencies available, without changing HEAD.
    pub async fn preflight_promote(
        &self,
        tenant: &str,
        kb: &str,
        revision: u64,
    ) -> Result<RevisionManifest> {
        let current = self
            .get_head(tenant, kb)
            .await?
            .context("knowledge base has no HEAD")?;
        let manifest = if current.0.revision == revision {
            self.get_manifest_verified(tenant, kb, revision, &current.0.manifest_sha256)
                .await?
        } else {
            let marker = self.get_commit_marker(tenant, kb, revision).await?;
            self.get_manifest_verified(tenant, kb, revision, &marker.manifest_sha256)
                .await?
        };
        self.verify_references(&manifest).await?;
        Ok(manifest)
    }

    pub async fn promote(&self, tenant: &str, kb: &str, revision: u64) -> Result<()> {
        let current = self
            .get_head(tenant, kb)
            .await?
            .context("knowledge base has no HEAD")?;
        if current.0.revision == revision {
            return Ok(());
        }

        // Repair the commit marker for the currently-visible revision before moving
        // HEAD away. This closes the crash window where publish had completed its
        // HEAD CAS but died before writing the marker.
        let current_manifest = self
            .get_manifest_verified(tenant, kb, current.0.revision, &current.0.manifest_sha256)
            .await?;
        self.ensure_commit_marker(&current_manifest, &current.0.manifest_sha256)
            .await?;

        // Promotions may move backward or forward across revisions that were
        // previously committed. Failed-CAS orphan manifests have no commit marker
        // and therefore cannot be promoted into visibility.
        let marker = self.get_commit_marker(tenant, kb, revision).await?;
        let manifest = self
            .get_manifest_verified(tenant, kb, revision, &marker.manifest_sha256)
            .await?;
        self.verify_references(&manifest).await?;

        let body = Bytes::from(serde_json::to_vec(&Head {
            revision,
            manifest_key: marker.manifest_key,
            manifest_sha256: marker.manifest_sha256,
        })?);
        let etag = current.1.context("missing HEAD etag")?;
        if self
            .store
            .put(
                &Self::head_key(tenant, kb)?,
                body,
                PutCondition::MatchEtag(etag),
            )
            .await?
            .is_none()
        {
            bail!("concurrent HEAD update")
        }
        Ok(())
    }
}

fn default_embedding_provider() -> String {
    "external".to_owned()
}

pub fn validate_scope_component(name: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 128 {
        bail!("{name} must be 1..=128 bytes")
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        bail!("{name} contains unsupported characters")
    }
    Ok(())
}

fn validate_manifest_shape(m: &RevisionManifest) -> Result<()> {
    validate_scope_component("tenant", &m.tenant)?;
    validate_scope_component("knowledge_base", &m.knowledge_base)?;
    if m.revision == 0 {
        bail!("revision must be positive")
    }
    if let Some(parent) = m.parent_revision {
        if parent == 0 || parent >= m.revision {
            bail!("parent_revision must be positive and strictly less than revision")
        }
    }
    if m.dimension == 0 || m.dimension > 65_536 {
        bail!("invalid embedding dimension")
    }
    if !matches!(m.embedding_provider.as_str(), "openrouter" | "external") {
        bail!("invalid embedding_provider")
    }
    if m.embedding_model.is_empty() || m.embedding_model.len() > 512 {
        bail!("invalid embedding_model")
    }
    if m.shards.is_empty() || m.shards.len() > 4_096 {
        bail!("revision must contain 1..=4096 shards")
    }
    let mut keys = BTreeSet::new();
    for s in &m.shards {
        if !keys.insert(&s.key) {
            bail!("duplicate shard {}", s.key)
        }
        crate::object_store::validate_object_key(&s.key)?;
        validate_digest(&s.sha256)?;
        validate_digest(&s.directory_sha256)?;
        if s.size < crate::binary::HEADER_SIZE as u64
            || s.format_version != crate::binary::FORMAT_VERSION
        {
            bail!("invalid shard descriptor")
        }
    }
    crate::object_store::validate_object_key(&m.stats.key)?;
    validate_digest(&m.stats.sha256)?;
    validate_digest(&m.stats.raw_sha256)?;
    if m.stats.size == 0 || m.stats.size > MAX_STATS_COMPRESSED {
        bail!("invalid revision stats descriptor")
    }
    if m.tombstones.len() > 4_096 {
        bail!("revision contains too many tombstone objects")
    }
    let mut tombstone_keys = BTreeSet::new();
    for t in &m.tombstones {
        if !tombstone_keys.insert(&t.key) {
            bail!("duplicate tombstone object {}", t.key)
        }
        crate::object_store::validate_object_key(&t.key)?;
        validate_digest(&t.sha256)?;
        if t.size == 0 || t.size > MAX_TOMBSTONE_COMPRESSED || t.count == 0 {
            bail!("invalid tombstone descriptor")
        }
    }
    if let Some(uqa) = &m.uqa_bundle {
        crate::object_store::validate_object_key(&uqa.key)?;
        validate_digest(&uqa.sha256)?;
        if uqa.size == 0 {
            bail!("invalid UQA bundle size")
        }
        if uqa.materialized_revision != m.revision {
            bail!("UQA bundle materialized_revision must equal manifest revision")
        }
        validate_database_filename(&uqa.database_filename)?;
    }
    Ok(())
}

fn validate_database_filename(name: &str) -> Result<()> {
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

fn ensure_content_addressed_name(key: &str, sha256: &str) -> Result<()> {
    let file = key
        .rsplit('/')
        .next()
        .context("object key has no filename")?;
    if file != sha256 && !file.starts_with(&format!("{sha256}.")) {
        bail!("content-addressed key {key} does not match declared SHA-256")
    }
    Ok(())
}

fn validate_digest(s: &str) -> Result<()> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid SHA-256 digest")
    }
    Ok(())
}

pub async fn upload_tombstones(
    store: Arc<dyn ObjectStore>,
    prefix: &str,
    ids: impl IntoIterator<Item = String>,
) -> Result<TombstoneDescriptor> {
    let mut ids: Vec<String> = ids.into_iter().collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        bail!("refusing to upload an empty tombstone object")
    }
    for id in &ids {
        if id.is_empty() || id.len() > 4096 {
            bail!("invalid tombstone id")
        }
    }
    let raw = serde_json::to_vec(&ids)?;
    if raw.len() as u64 > MAX_TOMBSTONE_RAW {
        bail!("tombstone object exceeds raw size limit")
    }
    let compressed = zstd::stream::encode_all(raw.as_slice(), 3)?;
    if compressed.len() as u64 > MAX_TOMBSTONE_COMPRESSED {
        bail!("tombstone object exceeds compressed size limit")
    }
    let bytes = Bytes::from(compressed);
    let sha = hex_sha256(&bytes);
    let key = format!("{prefix}/{sha}.tombstones.json.zst");
    let size = bytes.len() as u64;
    ensure_create_only_object(store.clone(), &key, bytes, size).await?;
    Ok(TombstoneDescriptor {
        key,
        sha256: sha,
        size,
        count: ids.len() as u64,
    })
}

fn decode_tombstone_object(
    bytes: Bytes,
    descriptor: TombstoneDescriptor,
) -> Result<(Vec<String>, u64)> {
    if bytes.len() as u64 != descriptor.size || hex_sha256(&bytes) != descriptor.sha256 {
        bail!("tombstone object integrity mismatch: {}", descriptor.key)
    }
    let mut decoder = zstd::stream::read::Decoder::new(bytes.as_ref())?;
    let mut raw = Vec::new();
    decoder
        .by_ref()
        .take(MAX_TOMBSTONE_RAW + 1)
        .read_to_end(&mut raw)?;
    if raw.len() as u64 > MAX_TOMBSTONE_RAW {
        bail!("tombstone object exceeds decompression limit")
    }
    let ids: Vec<String> = serde_json::from_slice(&raw)?;
    if ids.len() as u64 != descriptor.count {
        bail!("tombstone count mismatch: {}", descriptor.key)
    }
    for id in &ids {
        if id.is_empty() || id.len() > 4096 {
            bail!("invalid tombstone id in {}", descriptor.key)
        }
    }
    if ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        bail!(
            "tombstones must be strictly sorted and unique: {}",
            descriptor.key
        )
    }
    Ok((ids, raw.len() as u64))
}

pub async fn read_tombstones(
    store: Arc<dyn ObjectStore>,
    descriptors: &[TombstoneDescriptor],
) -> Result<(std::collections::HashSet<String>, u64, u64)> {
    let mut out = std::collections::HashSet::new();
    let mut bytes_read = 0u64;
    let mut reads = 0u64;
    let mut raw_total = 0u64;
    let mut id_total = 0u64;
    for descriptor in descriptors {
        if descriptor.size == 0 || descriptor.size > MAX_TOMBSTONE_COMPRESSED {
            bail!("invalid tombstone compressed size")
        }
        let bytes = store.get_range(&descriptor.key, 0, descriptor.size).await?;
        reads = reads.saturating_add(1);
        bytes_read = bytes_read.saturating_add(bytes.len() as u64);
        let descriptor = descriptor.clone();
        let (ids, raw_len) =
            tokio::task::spawn_blocking(move || decode_tombstone_object(bytes, descriptor))
                .await
                .context("join tombstone decode task")??;
        raw_total = raw_total
            .checked_add(raw_len)
            .context("tombstone raw-size total overflow")?;
        if raw_total > MAX_TOMBSTONE_RAW_TOTAL {
            bail!("revision tombstones exceed total decompressed-size limit")
        }
        id_total = id_total
            .checked_add(u64::try_from(ids.len()).context("tombstone count exceeds u64")?)
            .context("tombstone total count overflow")?;
        if id_total > MAX_TOMBSTONE_IDS_TOTAL {
            bail!("revision tombstones exceed total id-count limit")
        }
        out.extend(ids);
    }
    Ok((out, bytes_read, reads))
}

pub async fn upload_revision_stats(
    store: Arc<dyn ObjectStore>,
    prefix: &str,
    stats: &RevisionStats,
) -> Result<RevisionStatsDescriptor> {
    stats.validate()?;
    let raw = serde_json::to_vec(stats)?;
    if raw.len() as u64 > MAX_STATS_RAW {
        bail!("revision stats exceed raw size limit")
    }
    let raw_sha256 = hex_sha256(&raw);
    let compressed = zstd::stream::encode_all(raw.as_slice(), 3)?;
    if compressed.len() as u64 > MAX_STATS_COMPRESSED {
        bail!("revision stats exceed compressed size limit")
    }
    let bytes = Bytes::from(compressed);
    let sha256 = hex_sha256(&bytes);
    let size = bytes.len() as u64;
    let key = format!("{prefix}/{sha256}.stats.json.zst");
    ensure_create_only_object(store, &key, bytes, size).await?;
    Ok(RevisionStatsDescriptor {
        key,
        sha256,
        size,
        raw_sha256,
    })
}

fn decode_revision_stats_object(
    bytes: Bytes,
    descriptor: RevisionStatsDescriptor,
) -> Result<RevisionStats> {
    if bytes.len() as u64 != descriptor.size || hex_sha256(&bytes) != descriptor.sha256 {
        bail!("revision stats integrity mismatch")
    }
    let mut decoder = zstd::stream::read::Decoder::new(bytes.as_ref())?;
    let mut raw = Vec::new();
    decoder
        .by_ref()
        .take(MAX_STATS_RAW + 1)
        .read_to_end(&mut raw)?;
    if raw.len() as u64 > MAX_STATS_RAW {
        bail!("revision stats exceed decompression limit")
    }
    if hex_sha256(&raw) != descriptor.raw_sha256 {
        bail!("revision stats raw SHA-256 mismatch")
    }
    let stats: RevisionStats = serde_json::from_slice(&raw)?;
    stats.validate()?;
    Ok(stats)
}

pub async fn read_revision_stats(
    store: Arc<dyn ObjectStore>,
    descriptor: &RevisionStatsDescriptor,
) -> Result<(RevisionStats, u64, u64)> {
    if descriptor.size == 0 || descriptor.size > MAX_STATS_COMPRESSED {
        bail!("invalid revision stats compressed size")
    }
    let bytes = store.get_range(&descriptor.key, 0, descriptor.size).await?;
    let bytes_len = bytes.len() as u64;
    let descriptor = descriptor.clone();
    let stats =
        tokio::task::spawn_blocking(move || decode_revision_stats_object(bytes, descriptor))
            .await
            .context("join revision-stats decode task")??;
    Ok((stats, bytes_len, 1))
}

pub async fn upload_uqa_bundle_file(
    store: Arc<dyn ObjectStore>,
    prefix: &str,
    path: &std::path::Path,
    materialized_revision: u64,
    database_filename: impl Into<String>,
) -> Result<UqaBundleDescriptor> {
    if materialized_revision == 0 {
        bail!("materialized revision must be positive")
    }
    let database_filename = database_filename.into();
    validate_database_filename(&database_filename)?;
    let size = tokio::fs::metadata(path).await?.len();
    if size == 0 {
        bail!("UQA database is empty")
    }
    let sha256 = crate::object_store::sha256_file(path).await?;
    let key = format!("{prefix}/{sha256}.uqa");
    match store.put_file(&key, path, PutCondition::CreateOnly).await? {
        Some(meta) => {
            if meta.size != size {
                bail!("uploaded UQA bundle size mismatch")
            }
        }
        None => {
            let meta = store
                .head(&key)
                .await?
                .context("content-addressed UQA object exists but HEAD failed")?;
            if meta.size != size {
                bail!("existing UQA object has wrong size")
            }
            verify_remote_object_hash(store.clone(), &key, &sha256, size).await?;
        }
    }
    verify_local_file_unchanged(path, size, &sha256).await?;
    Ok(UqaBundleDescriptor {
        materialized_revision,
        key,
        sha256,
        size,
        database_filename,
    })
}

pub async fn upload_content_addressed(
    store: Arc<dyn ObjectStore>,
    prefix: &str,
    bytes: Bytes,
    extension: &str,
) -> Result<ShardDescriptor> {
    let header = crate::binary::Header::decode(
        bytes
            .get(..crate::binary::HEADER_SIZE)
            .context("shard too small")?,
    )?;
    let sha = hex_sha256(&bytes);
    let key = format!("{prefix}/{sha}.{extension}");
    let size = bytes.len() as u64;
    ensure_create_only_object(store, &key, bytes, size).await?;
    Ok(ShardDescriptor {
        key,
        sha256: sha,
        size,
        format_version: header.version,
        directory_sha256: header.directory_sha256_hex(),
    })
}

pub async fn upload_content_addressed_file(
    store: Arc<dyn ObjectStore>,
    prefix: &str,
    path: &std::path::Path,
    extension: &str,
) -> Result<ShardDescriptor> {
    let (header, size) = crate::index::reader::read_header(path)?;
    let sha = crate::object_store::sha256_file(path).await?;
    let key = format!("{prefix}/{sha}.{extension}");
    match store.put_file(&key, path, PutCondition::CreateOnly).await? {
        Some(meta) => {
            if meta.size != size {
                bail!("uploaded shard size mismatch")
            }
        }
        None => {
            let meta = store
                .head(&key)
                .await?
                .context("content-addressed shard exists but HEAD failed")?;
            if meta.size != size {
                bail!("existing content-addressed shard has wrong size")
            }
            verify_remote_object_hash(store.clone(), &key, &sha, size).await?;
        }
    }
    verify_local_file_unchanged(path, size, &sha).await?;
    Ok(ShardDescriptor {
        key,
        sha256: sha,
        size,
        format_version: header.version,
        directory_sha256: header.directory_sha256_hex(),
    })
}

async fn ensure_create_only_object(
    store: Arc<dyn ObjectStore>,
    key: &str,
    bytes: Bytes,
    expected_size: u64,
) -> Result<()> {
    let expected_sha = hex_sha256(&bytes);
    match store.put(key, bytes, PutCondition::CreateOnly).await? {
        Some(meta) => {
            if meta.size != expected_size {
                bail!("uploaded object size mismatch")
            }
        }
        None => {
            let meta = store
                .head(key)
                .await?
                .context("content-addressed object exists but HEAD failed")?;
            if meta.size != expected_size {
                bail!("existing content-addressed object has wrong size")
            }
            verify_remote_object_hash(store, key, &expected_sha, expected_size).await?;
        }
    }
    Ok(())
}

async fn verify_local_file_unchanged(
    path: &std::path::Path,
    expected_size: u64,
    expected_sha: &str,
) -> Result<()> {
    let size = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("re-inspect local upload source {}", path.display()))?
        .len();
    if size != expected_size {
        bail!(
            "local upload source changed size while uploading: {}",
            path.display()
        )
    }
    let actual = crate::object_store::sha256_file(path).await?;
    if actual != expected_sha {
        bail!(
            "local upload source changed content while uploading: {}",
            path.display()
        )
    }
    Ok(())
}

async fn verify_remote_object_hash(
    store: Arc<dyn ObjectStore>,
    key: &str,
    expected_sha: &str,
    expected_size: u64,
) -> Result<()> {
    // Avoid loading a multi-GB existing shard/UQA object into memory merely to
    // verify a create-only collision. Stream it to a temporary file and hash it.
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("object");
    let downloaded = store.download_to(key, &path, expected_size).await?;
    if downloaded != expected_size {
        bail!("existing object size changed while verifying {key}")
    }
    let actual = crate::object_store::sha256_file(&path).await?;
    if actual != expected_sha {
        bail!("existing content-addressed object hash mismatch for {key}")
    }
    Ok(())
}
