use cairn_uqa::config::{write_default_config, CairnConfig, StoreOverrides};

#[test]
fn default_config_round_trips_and_prefers_cli_scope() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    let written = write_default_config(&path, Some("tenant-a"), Some("kb-a"), false)?;
    assert_eq!(written.tenant(None), Some("tenant-a"));
    assert_eq!(written.knowledge_base(None), Some("kb-a"));

    let raw = std::fs::read_to_string(&path)?;
    let loaded: CairnConfig = toml::from_str(&raw)?;
    loaded.validate()?;
    assert_eq!(loaded.tenant(Some("tenant-b")), Some("tenant-b"));
    assert_eq!(loaded.knowledge_base(Some("kb-b")), Some("kb-b"));
    assert_eq!(loaded.embedding.provider, "openrouter");
    assert_eq!(loaded.embedding.model, "qwen/qwen3-embedding-8b");
    assert_eq!(loaded.embedding.dimensions, 1024);
    Ok(())
}

#[test]
fn config_rejects_two_store_backends() {
    let config = CairnConfig {
        store: cairn_uqa::config::StoreConfig {
            local: Some("local".into()),
            object_gateway: Some("https://example.com".into()),
            bearer_env: None,
        },
        embedding: Default::default(),
        defaults: Default::default(),
    };
    assert!(config.validate().is_err());
}

#[test]
fn local_store_is_the_zero_configuration_default() -> anyhow::Result<()> {
    let config = CairnConfig::default();
    let _store = config.resolve_store(&StoreOverrides::default())?;
    Ok(())
}

#[tokio::test]
async fn local_store_create_only_is_serialized_across_instances() -> anyhow::Result<()> {
    use bytes::Bytes;
    use cairn_uqa::object_store::{LocalStore, ObjectStore, PutCondition};

    let dir = tempfile::tempdir()?;
    let left = LocalStore::new(dir.path());
    let right = LocalStore::new(dir.path());
    let (a, b) = tokio::join!(
        left.put("cas/value", Bytes::from_static(b"a"), PutCondition::CreateOnly),
        right.put("cas/value", Bytes::from_static(b"b"), PutCondition::CreateOnly),
    );
    let successes = [a?, b?].into_iter().filter(Option::is_some).count();
    assert_eq!(successes, 1);
    Ok(())
}

#[tokio::test]
async fn cli_backend_override_replaces_configured_backend() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let config = CairnConfig {
        store: cairn_uqa::config::StoreConfig {
            local: None,
            object_gateway: Some("https://example.invalid".to_owned()),
            bearer_env: None,
        },
        embedding: Default::default(),
        defaults: Default::default(),
    };
    let store = config.resolve_store(&StoreOverrides {
        local: Some(temp.path().to_path_buf()),
        object_gateway: None,
        bearer_env: None,
    })?;
    assert!(store.head("missing/object").await?.is_none());
    Ok(())
}

#[test]
fn config_rejects_plain_http_lookalike_localhost_gateway() {
    let config = CairnConfig {
        store: cairn_uqa::config::StoreConfig {
            local: None,
            object_gateway: Some("http://localhost.evil.example".to_owned()),
            bearer_env: None,
        },
        embedding: Default::default(),
        defaults: Default::default(),
    };
    assert!(config.validate().is_err());
}

#[test]
fn config_rejects_unknown_fields_in_v2() {
    let raw = r#"
[store]
local = "store"

[embedding]
provider = "openrouter"
model = "qwen/qwen3-embedding-8b"
dimensions = 1024
dimensons = 1024
"#;
    assert!(toml::from_str::<CairnConfig>(raw).is_err());
}
