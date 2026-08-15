use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    fs,
    process::{Command, Output},
};

fn cairn(args: &[&str], cwd: &std::path::Path) -> Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_cairn"))
        .args(args)
        .current_dir(cwd)
        .env_remove("CAIRN_CONFIG")
        .env_remove("CAIRN_TENANT")
        .env_remove("CAIRN_KB")
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .context("run cairn CLI")
}

fn assert_success(output: Output) -> Result<Output> {
    anyhow::ensure!(
        output.status.success(),
        "CLI failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(output)
}

#[test]
fn help_prioritizes_the_one_command_snapshot_workflow() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let output = assert_success(cairn(&["--help"], temp.path())?)?;
    let help = String::from_utf8(output.stdout)?;
    assert!(help.contains("snapshot"));
    assert!(help.contains("embed"));
    assert!(help.contains("doctor"));
    assert!(help.contains("Quick start:"));
    assert!(help.contains("ingest"));
    assert!(help.contains("query"));
    Ok(())
}

#[test]
fn init_doctor_snapshot_head_and_search_work_end_to_end() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let config = temp.path().join(".cairn/config.toml");
    let corpus = temp.path().join("chunks.jsonl");
    fs::write(
        &corpus,
        concat!(
            "{\"id\":\"doc-v1-c0\",\"text\":\"hello object storage\",\"vector\":[1.0,0.0],\"metadata\":{\"lang\":\"en\"}}\n",
            "{\"id\":\"doc-v1-c1\",\"text\":\"functional rust retrieval\",\"vector\":[0.0,1.0],\"metadata\":{\"lang\":\"en\"}}\n",
        ),
    )?;

    assert_success(cairn(
        &["init", "--tenant", "acme", "--kb", "handbook"],
        temp.path(),
    )?)?;
    assert!(config.exists());

    assert_success(cairn(&["doctor"], temp.path())?)?;

    assert_success(cairn(
        &[
            "snapshot",
            "chunks.jsonl",
            "--embedding-model",
            "test-embedding",
            "--dev-calibration",
            "--shards",
            "1",
        ],
        temp.path(),
    )?)?;

    let head = assert_success(cairn(&["head"], temp.path())?)?;
    assert_eq!(String::from_utf8(head.stdout)?.trim(), "1");

    let search = assert_success(cairn(
        &[
            "--format",
            "json",
            "search",
            "object storage",
            "--limit",
            "1",
            "--lexical-only",
        ],
        temp.path(),
    )?)?;
    let response: Value = serde_json::from_slice(&search.stdout)?;
    assert_eq!(response.get("revision").and_then(Value::as_u64), Some(1));
    assert_eq!(
        response.get("embedding_provider").and_then(Value::as_str),
        Some("external")
    );
    assert_eq!(
        response.get("embedding_model").and_then(Value::as_str),
        Some("test-embedding")
    );
    assert_eq!(response.get("dimension").and_then(Value::as_u64), Some(2));
    assert!(response
        .get("corpus_sha256")
        .and_then(Value::as_str)
        .is_some_and(|value| value.len() == 64));
    assert_eq!(
        response.get("hits").and_then(Value::as_array).map(Vec::len),
        Some(1)
    );

    fs::write(temp.path().join("query.txt"), "functional rust")?;
    let from_file = assert_success(cairn(
        &[
            "--format",
            "json",
            "search",
            "--query-file",
            "query.txt",
            "--limit",
            "1",
            "--lexical-only",
        ],
        temp.path(),
    )?)?;
    let response: Value = serde_json::from_slice(&from_file.stdout)?;
    assert_eq!(
        response.get("hits").and_then(Value::as_array).map(Vec::len),
        Some(1)
    );
    Ok(())
}

#[test]
fn text_only_snapshot_explains_missing_openrouter_key() -> Result<()> {
    let temp = tempfile::tempdir()?;
    fs::write(
        temp.path().join("chunks.jsonl"),
        "{\"id\":\"doc-v1-c0\",\"text\":\"hello object storage\"}\n",
    )?;
    assert_success(cairn(
        &["init", "--tenant", "acme", "--kb", "handbook"],
        temp.path(),
    )?)?;
    let output = cairn(
        &["snapshot", "chunks.jsonl", "--dev-calibration"],
        temp.path(),
    )?;
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("OPENROUTER_API_KEY"));
    Ok(())
}

#[test]
fn doctor_help_exposes_opt_in_live_embedding_check() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let output = assert_success(cairn(&["doctor", "--help"], temp.path())?)?;
    let help = String::from_utf8(output.stdout)?;
    assert!(help.contains("--check-embedding"));
    assert!(help.contains("--check-write"));
    assert!(help.contains("--full"));
    Ok(())
}

#[test]
fn snapshot_after_rollback_uses_a_fresh_revision_number() -> Result<()> {
    let temp = tempfile::tempdir()?;
    fs::write(
        temp.path().join("chunks.jsonl"),
        concat!(
            "{\"id\":\"a\",\"text\":\"alpha\",\"vector\":[1.0,0.0]}\n",
            "{\"id\":\"b\",\"text\":\"beta\",\"vector\":[0.0,1.0]}\n",
        ),
    )?;
    assert_success(cairn(
        &["init", "--tenant", "acme", "--kb", "handbook"],
        temp.path(),
    )?)?;
    let snapshot = || {
        cairn(
            &[
                "snapshot",
                "chunks.jsonl",
                "--embedding-model",
                "test-embedding",
                "--dev-calibration",
                "--shards",
                "1",
            ],
            temp.path(),
        )
    };

    assert_success(snapshot()?)?;
    assert_success(snapshot()?)?;
    assert_success(cairn(&["promote", "1"], temp.path())?)?;

    let dry_run = assert_success(cairn(&["promote", "2", "--dry-run"], temp.path())?)?;
    assert!(String::from_utf8(dry_run.stdout)?.contains("dry run"));

    assert_success(snapshot()?)?;
    let head = assert_success(cairn(&["head"], temp.path())?)?;
    assert_eq!(String::from_utf8(head.stdout)?.trim(), "3");
    Ok(())
}

#[test]
fn config_command_is_secret_safe_and_machine_readable() -> Result<()> {
    let temp = tempfile::tempdir()?;
    assert_success(cairn(
        &["init", "--tenant", "acme", "--kb", "handbook"],
        temp.path(),
    )?)?;
    let output = assert_success(cairn(&["--format", "json", "config"], temp.path())?)?;
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        value
            .pointer("/config/defaults/tenant")
            .and_then(Value::as_str),
        Some("acme")
    );
    assert_eq!(
        value
            .pointer("/config/embedding/provider")
            .and_then(Value::as_str),
        Some("openrouter")
    );
    assert!(value.get("embedding_api_key_set").is_some());
    assert!(!String::from_utf8(output.stdout)?.contains("sk-or-"));
    Ok(())
}

#[test]
fn serve_help_exposes_harness_security_controls() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let output = assert_success(cairn(&["serve", "--help"], temp.path())?)?;
    let help = String::from_utf8(output.stdout)?;
    assert!(help.contains("--auth-token-env"));
    assert!(help.contains("--restrict-to-default-scope"));
    assert!(help.contains("--allowed-tenant"));
    assert!(help.contains("--allow-unauthenticated"));
    Ok(())
}
