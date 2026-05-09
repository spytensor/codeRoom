//! Real-`claude` smoke test for the Claude Code adapter.
//!
//! `#[ignore]`'d so it does not run in default `cargo test`. Invoke with:
//!
//! ```bash
//! cargo test --test cc_adapter_smoke -- --ignored
//! ```
//!
//! The test spawns `claude` with a tiny priors file, sends a single
//! "say HELLO" prompt, and asserts the resulting CREP stream contains:
//!
//! - exactly one `RoleStarted` (with our role name + `cc` engine)
//! - at least one `RoleSpoke` whose `text` contains `HELLO`
//! - exactly one `RoleStopped`
//!
//! Cost: ~$0.05–0.10 per run (one short Opus turn with cache priming).

use std::path::PathBuf;
use std::time::Duration;

use coderoom::adapter::cc::CcAdapter;
use coderoom::adapter::{Engine, EngineAdapter, RoleConfig, UserMessage};
use coderoom::crep::CrepEvent;
use tokio::time::timeout;

#[tokio::test]
#[ignore = "spawns real `claude` and burns API tokens"]
async fn cc_smoke_says_hello() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let priors_path: PathBuf = tmp.path().join("priors.md");
    tokio::fs::write(
        &priors_path,
        "You are a terse test fixture. Always reply with exactly one word.\n",
    )
    .await
    .expect("write priors");

    let adapter = CcAdapter::new();
    let config = RoleConfig {
        name: "smoke".to_owned(),
        engine: Engine::Cc,
        model: None,
        priors_path,
        budget_usd: 0.50,
    };

    let mut handle = adapter.start(config).await.expect("start");

    handle
        .tx_user
        .send(UserMessage::Prompt(
            "Reply with exactly the word: HELLO".to_owned(),
        ))
        .await
        .expect("send prompt");

    let mut role_started_seen = false;
    let mut role_spoke_seen = false;
    let mut role_stopped_seen = false;
    let mut spoke_text = String::new();

    // Drop the user channel so the engine treats stdin as EOF after the
    // prompt is consumed; the subprocess will then exit cleanly.
    drop(handle.tx_user);

    let collect = async {
        while let Some(event) = handle.rx_events.recv().await {
            match event {
                CrepEvent::RoleStarted { role, engine, .. } => {
                    assert_eq!(role, "smoke");
                    assert_eq!(engine, "cc");
                    role_started_seen = true;
                }
                CrepEvent::RoleSpoke { text, .. } => {
                    spoke_text = text;
                    role_spoke_seen = true;
                }
                CrepEvent::RoleStopped { .. } => {
                    role_stopped_seen = true;
                    break;
                }
                _ => {}
            }
        }
    };

    timeout(Duration::from_secs(120), collect)
        .await
        .expect("smoke test timed out — claude unresponsive?");

    assert!(role_started_seen, "expected RoleStarted");
    assert!(role_spoke_seen, "expected RoleSpoke");
    assert!(role_stopped_seen, "expected RoleStopped");
    assert!(
        spoke_text.contains("HELLO"),
        "expected reply to contain HELLO, got: {spoke_text:?}"
    );
}
