use crate::{
    embedding::EmbeddingSettings,
    object_store::{HttpStore, LocalStore, ObjectStore},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_CONFIG_PATH: &str = ".cairn/config.toml";
pub const DEFAULT_LOCAL_STORE: &str = ".cairn/store";
pub const DEFAULT_CACHE: &str = ".cairn/cache";
pub const DEFAULT_BEARER_ENV: &str = "CAIRN_BEARER_TOKEN";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CairnConfig {
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub embedding: EmbeddingSettings,
    #[serde(default)]
    pub defaults: DefaultsConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    pub local: Option<PathBuf>,
    pub object_gateway: Option<String>,
    /// Name of the environment variable containing the bearer token. Keeping
    /// secrets out of config files and shell history is the default UX.
    pub bearer_env: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultsConfig {
    pub tenant: Option<String>,
    pub knowledge_base: Option<String>,
    pub embedding_model: Option<String>,
    pub cache: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct StoreOverrides {
    pub local: Option<PathBuf>,
    pub object_gateway: Option<String>,
    pub bearer_env: Option<String>,
}

enum StoreSelection {
    Local(PathBuf),
    Gateway(String),
}

impl CairnConfig {
    pub fn load(path: Option<&Path>) -> Result<(Self, Option<PathBuf>)> {
        let explicit = path
            .map(Path::to_path_buf)
            .or_else(|| env::var_os("CAIRN_CONFIG").map(PathBuf::from));
        let candidate = explicit
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        if !candidate.exists() {
            if explicit.is_some() {
                bail!("config file does not exist: {}", candidate.display());
            }
            return Ok((Self::default(), None));
        }
        let mut file = std::fs::File::open(&candidate)
            .with_context(|| format!("open config {}", candidate.display()))?;
        let mut raw = String::new();
        use std::io::Read as _;
        file.by_ref()
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut raw)
            .with_context(|| format!("read config {}", candidate.display()))?;
        if raw.len() as u64 > MAX_CONFIG_BYTES {
            bail!(
                "config file exceeds {MAX_CONFIG_BYTES} bytes: {}",
                candidate.display()
            )
        }
        let mut config: Self = toml::from_str(&raw)
            .with_context(|| format!("parse config {}", candidate.display()))?;
        config.validate()?;
        let base = candidate
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        config.resolve_relative_paths(base);
        Ok((config, Some(candidate)))
    }

    fn resolve_relative_paths(&mut self, base: &Path) {
        if let Some(path) = self.store.local.as_mut() {
            if path.is_relative() {
                *path = base.join(&*path);
            }
        }
        if let Some(path) = self.defaults.cache.as_mut() {
            if path.is_relative() {
                *path = base.join(&*path);
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.store.local.is_some() && self.store.object_gateway.is_some() {
            bail!("config.store must select either local or object_gateway, not both");
        }
        if let Some(url) = &self.store.object_gateway {
            validate_service_url("object_gateway", url)?;
        }
        if let Some(name) = &self.store.bearer_env {
            validate_env_name(name)?;
        }
        self.embedding.validate()?;
        Ok(())
    }

    #[must_use]
    pub fn tenant<'a>(&'a self, cli: Option<&'a str>) -> Option<&'a str> {
        cli.or(self.defaults.tenant.as_deref())
    }

    #[must_use]
    pub fn knowledge_base<'a>(&'a self, cli: Option<&'a str>) -> Option<&'a str> {
        cli.or(self.defaults.knowledge_base.as_deref())
    }

    #[must_use]
    pub fn cache_path(&self, cli: Option<&Path>) -> PathBuf {
        cli.map(Path::to_path_buf)
            .or_else(|| self.defaults.cache.clone())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CACHE))
    }

    pub fn resolve_store(&self, overrides: &StoreOverrides) -> Result<Arc<dyn ObjectStore>> {
        // Backend flags are true overrides, not additive fields. In particular,
        // `--local-store` must be able to replace an object_gateway from config
        // without forcing developers to edit the config first.
        let selected = match (&overrides.local, &overrides.object_gateway) {
            (Some(_), Some(_)) => {
                bail!("select either a local store or an object gateway, not both")
            }
            (Some(path), None) => StoreSelection::Local(path.clone()),
            (None, Some(url)) => StoreSelection::Gateway(url.clone()),
            (None, None) => match (&self.store.local, &self.store.object_gateway) {
                (Some(_), Some(_)) => {
                    bail!("config.store must select either local or object_gateway, not both")
                }
                (Some(path), None) => StoreSelection::Local(path.clone()),
                (None, Some(url)) => StoreSelection::Gateway(url.clone()),
                (None, None) => StoreSelection::Local(PathBuf::from(DEFAULT_LOCAL_STORE)),
            },
        };
        match selected {
            StoreSelection::Local(path) => Ok(Arc::new(LocalStore::new(path))),
            StoreSelection::Gateway(url) => {
                let env_name = overrides
                    .bearer_env
                    .as_deref()
                    .or(self.store.bearer_env.as_deref())
                    .unwrap_or(DEFAULT_BEARER_ENV);
                validate_env_name(env_name)?;
                let bearer = env::var(env_name).ok().filter(|value| !value.is_empty());
                Ok(Arc::new(HttpStore::new(url, bearer)?))
            }
        }
    }
}

pub fn write_default_config(
    path: &Path,
    tenant: Option<&str>,
    kb: Option<&str>,
    force: bool,
) -> Result<CairnConfig> {
    if path.exists() && !force {
        bail!(
            "config already exists at {}; pass --force to replace it",
            path.display()
        );
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let config = CairnConfig {
        store: StoreConfig {
            local: Some(PathBuf::from("store")),
            object_gateway: None,
            bearer_env: Some(DEFAULT_BEARER_ENV.to_owned()),
        },
        embedding: EmbeddingSettings::default(),
        defaults: DefaultsConfig {
            tenant: tenant.map(str::to_owned),
            knowledge_base: kb.map(str::to_owned),
            embedding_model: None,
            cache: Some(PathBuf::from("cache")),
        },
    };
    config.validate()?;
    let encoded = toml::to_string_pretty(&config)?;
    atomic_write(path, encoded.as_bytes())?;
    Ok(config)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write as _;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    sync_directory(parent)?;
    Ok(())
}

#[cfg(unix)]
pub fn sync_directory(path: &Path) -> Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
pub fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn validate_service_url(name: &str, raw: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(raw).with_context(|| format!("{name} is not a valid URL"))?;
    let host = parsed
        .host_str()
        .with_context(|| format!("{name} must include a host"))?;
    let local_http = parsed.scheme() == "http" && matches!(host, "localhost" | "127.0.0.1" | "::1");
    if parsed.scheme() != "https" && !local_http {
        bail!("{name} must use https:// (plain http is allowed only for localhost)")
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("{name} must not contain credentials")
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("{name} must not contain a query string or fragment")
    }
    Ok(())
}

pub fn validate_env_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && name
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_');
    if !valid {
        bail!("invalid bearer environment variable name: {name}")
    }
    Ok(())
}
