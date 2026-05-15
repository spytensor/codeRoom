//! Real-`codex` smoke test for the Codex adapter.
//!
//! `#[ignore]`'d so default `cargo test` does not run it. Invoke with:
//!
//! ```bash
//! cargo test --test codex_adapter_smoke -- --ignored
//! ```
//!
//! Requires `codex` to be on `PATH` and authenticated against an OpenAI
//! API key (via `OPENAI_API_KEY` or `codex login`).

use std::path::PathBuf;
use std::time::Duration;

use coderoom::adapter::codex::CodexAdapter;
use coderoom::adapter::{Engine, EngineAdapter, PermissionMode, RoleConfig, UserMessage};
use coderoom::crep::CrepEvent;
use tokio::time::timeout;

#[tokio::test]
#[ignore = "spawns real `codex mcp-server` and burns API tokens"]
async fn codex_smoke_says_hello() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let priors_path: PathBuf = tmp.path().join("priors.md");
    tokio::fs::write(
        &priors_path,
        "You are a terse test fixture. Always reply with exactly one word.\n",
    )
    .await
    .expect("write priors");

    let adapter = CodexAdapter::new();
    let config = RoleConfig {
        name: "smoke".to_owned(),
        engine: Engine::Codex,
        model: None,
        priors_path,
        permission_mode: PermissionMode::Bypass,
        permission_policy_path: None,
        permission_socket_path: None,
        resume_session_id: None,
    };

    let mut handle = adapter.start(config).await.expect("start");

    handle
        .tx_user
        .send(UserMessage::legacy_prompt(
            "Reply with exactly the word: HELLO",
        ))
        .await
        .expect("send prompt");

    drop(handle.tx_user);

    let mut role_started_seen = false;
    let mut role_spoke_seen = false;
    let mut spoke_text = String::new();

    let collect = async {
        while let Some(event) = handle.rx_events.recv().await {
            match event {
                CrepEvent::RoleStarted { role, engine, .. } => {
                    assert_eq!(role, "smoke");
                    assert_eq!(engine, "codex");
                    role_started_seen = true;
                }
                CrepEvent::RoleSpoke { text, .. } => {
                    spoke_text = text;
                    role_spoke_seen = true;
                    break;
                }
                CrepEvent::RoleStopped { .. } => break,
                _ => {}
            }
        }
    };

    timeout(Duration::from_secs(120), collect)
        .await
        .expect("smoke test timed out — codex unresponsive?");

    assert!(role_started_seen, "expected RoleStarted");
    assert!(role_spoke_seen, "expected RoleSpoke");
    assert!(
        spoke_text.contains("HELLO"),
        "expected reply to contain HELLO, got: {spoke_text:?}"
    );
}
