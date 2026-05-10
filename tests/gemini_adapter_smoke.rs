//! Real-`gemini` smoke test for the Gemini adapter.
//!
//! `#[ignore]`'d so default `cargo test` does not run it. Invoke with:
//!
//! ```bash
//! cargo test --test gemini_adapter_smoke -- --ignored
//! ```
//!
//! Requires `gemini` to be on `PATH` and authenticated (via
//! `GEMINI_API_KEY` or `gemini auth login`).

use std::path::PathBuf;
use std::time::Duration;

use coderoom::adapter::gemini::GeminiAdapter;
use coderoom::adapter::{Engine, EngineAdapter, PermissionMode, RoleConfig, UserMessage};
use coderoom::crep::CrepEvent;
use tokio::time::timeout;

#[tokio::test]
#[ignore = "spawns real `gemini` and burns API tokens"]
async fn gemini_smoke_says_hello() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let priors_path: PathBuf = tmp.path().join("priors.md");
    tokio::fs::write(
        &priors_path,
        "You are a terse test fixture. Always reply with exactly one word.\n",
    )
    .await
    .expect("write priors");

    let adapter = GeminiAdapter::new();
    let config = RoleConfig {
        name: "smoke".to_owned(),
        engine: Engine::Gemini,
        model: None,
        priors_path,
        budget_usd: 0.50,
        permission_mode: PermissionMode::Bypass,
        permission_policy_path: None,
    };

    let mut handle = adapter.start(config).await.expect("start");

    handle
        .tx_user
        .send(UserMessage::Prompt(
            "Reply with exactly the word: HELLO".to_owned(),
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
                    assert_eq!(engine, "gemini");
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
        .expect("smoke test timed out — gemini unresponsive?");

    assert!(role_started_seen, "expected RoleStarted");
    assert!(role_spoke_seen, "expected RoleSpoke");
    assert!(
        spoke_text.contains("HELLO"),
        "expected reply to contain HELLO, got: {spoke_text:?}"
    );
}
