use super::*;
use crate::config::ROLES_DIR;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn write_minimal_project(coderoom: &Path, body_extra: &str) {
    std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
    let body = format!(
        r#"
default_engine = "cc"
host_role = "host"

[roles.host]

{body_extra}
"#
    );
    std::fs::write(coderoom.join(CONFIG_FILE), body).unwrap();
    std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "host\n").unwrap();
}

fn write_user(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[test]
fn project_only_loads_unchanged() {
    let tmp = TempDir::new().unwrap();
    let coderoom = tmp.path().join(CODEROOM_DIR);
    write_minimal_project(&coderoom, "");
    let cfg = load(tmp.path(), None).expect("load");
    assert_eq!(cfg.default_engine, Engine::Cc);
    assert_eq!(cfg.permission_mode, PermissionMode::Ask);
    assert_eq!(cfg.host_role, "host");
}

#[test]
fn project_permission_mode_overrides_user_default() {
    let tmp = TempDir::new().unwrap();
    let user_path = tmp.path().join("user-config.toml");
    write_user(
        &user_path,
        r#"
[defaults]
permission_mode = "auto"
"#,
    );
    let coderoom = tmp.path().join(CODEROOM_DIR);
    std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
    std::fs::write(
        coderoom.join(CONFIG_FILE),
        r#"
default_engine = "cc"
permission_mode = "bypass"
host_role = "host"

[roles.host]
"#,
    )
    .unwrap();
    std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "host\n").unwrap();
    let cfg = load(tmp.path(), Some(&user_path)).expect("load");
    assert_eq!(cfg.permission_mode, PermissionMode::Bypass);
}

#[test]
fn user_default_engine_picked_when_project_omits_it() {
    let tmp = TempDir::new().unwrap();
    let user_path = tmp.path().join("user/config.toml");
    write_user(
        &user_path,
        r#"
[defaults]
engine = "codex"
"#,
    );
    let coderoom = tmp.path().join(CODEROOM_DIR);
    // Project intentionally omits default_engine.
    std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
    std::fs::write(
        coderoom.join(CONFIG_FILE),
        r#"
host_role = "host"

[roles.host]
"#,
    )
    .unwrap();
    std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "host\n").unwrap();

    let cfg = load(tmp.path(), Some(&user_path)).expect("load");
    assert_eq!(cfg.default_engine, Engine::Codex);
}

#[test]
fn legacy_user_budget_hint_is_accepted_but_ignored() {
    let tmp = TempDir::new().unwrap();
    let user_path = tmp.path().join("user/config.toml");
    write_user(
        &user_path,
        r#"
[defaults]
engine = "cc"
budget_per_role_usd = 0.5
"#,
    );
    let coderoom = tmp.path().join(CODEROOM_DIR);
    write_minimal_project(&coderoom, "");

    let cfg = load(tmp.path(), Some(&user_path)).expect("legacy budget hint should load");
    assert_eq!(cfg.default_engine, Engine::Cc);
}

#[test]
fn project_engine_overrides_user_engine() {
    let tmp = TempDir::new().unwrap();
    let user_path = tmp.path().join("user-config.toml");
    write_user(
        &user_path,
        r#"
[defaults]
engine = "codex"
"#,
    );
    let coderoom = tmp.path().join(CODEROOM_DIR);
    write_minimal_project(&coderoom, ""); // project: cc
    let cfg = load(tmp.path(), Some(&user_path)).expect("load");
    assert_eq!(cfg.default_engine, Engine::Cc);
}

#[test]
fn user_layer_with_roles_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let user_path = tmp.path().join("user-config.toml");
    write_user(&user_path, "[roles.backend]\n");
    let coderoom = tmp.path().join(CODEROOM_DIR);
    write_minimal_project(&coderoom, "");
    let err = load(tmp.path(), Some(&user_path)).expect_err("user roles must be rejected");
    match err {
        ConfigError::Forbidden { field, .. } => assert!(field.starts_with("[roles.")),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn user_layer_with_host_role_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let user_path = tmp.path().join("user-config.toml");
    write_user(&user_path, r#"host_role = "stolen""#);
    let coderoom = tmp.path().join(CODEROOM_DIR);
    write_minimal_project(&coderoom, "");
    let err = load(tmp.path(), Some(&user_path)).expect_err("user host_role must be rejected");
    match err {
        ConfigError::Forbidden { field, .. } => assert_eq!(field, "host_role"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn project_layer_with_engine_bin_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let coderoom = tmp.path().join(CODEROOM_DIR);
    std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
    std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "x").unwrap();
    std::fs::write(
        coderoom.join(CONFIG_FILE),
        r#"
default_engine = "cc"
host_role = "host"

[roles.host]

[engines.cc]
bin = "/opt/claude"
"#,
    )
    .unwrap();
    let err = load(tmp.path(), None).expect_err("project bin must be rejected");
    match err {
        ConfigError::Forbidden { field, .. } => assert_eq!(field, "engines.cc.bin"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn project_layer_with_engine_api_key_env_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let coderoom = tmp.path().join(CODEROOM_DIR);
    std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
    std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "x").unwrap();
    std::fs::write(
        coderoom.join(CONFIG_FILE),
        r#"
default_engine = "cc"
host_role = "host"

[roles.host]

[engines.cc]
api_key_env = "ANTHROPIC_API_KEY"
"#,
    )
    .unwrap();
    let err = load(tmp.path(), None).expect_err("project api_key_env must be rejected");
    match err {
        ConfigError::Forbidden { field, .. } => assert_eq!(field, "engines.cc.api_key_env"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn local_layer_can_carry_engine_bin() {
    let tmp = TempDir::new().unwrap();
    let coderoom = tmp.path().join(CODEROOM_DIR);
    write_minimal_project(&coderoom, "");
    std::fs::write(
        coderoom.join(CONFIG_LOCAL_FILE),
        r#"
[engines.cc]
bin = "/opt/claude"
"#,
    )
    .unwrap();
    let cfg = load(tmp.path(), None).expect("local layer with bin loads");
    assert_eq!(cfg.default_engine, Engine::Cc);
}

#[test]
fn missing_user_path_does_not_error() {
    let tmp = TempDir::new().unwrap();
    let coderoom = tmp.path().join(CODEROOM_DIR);
    write_minimal_project(&coderoom, "");
    let nonexistent = tmp.path().join("does-not-exist.toml");
    let cfg = load(tmp.path(), Some(&nonexistent)).expect("missing user is fine");
    assert_eq!(cfg.default_engine, Engine::Cc);
}

#[test]
fn missing_default_engine_in_all_layers_errors() {
    let tmp = TempDir::new().unwrap();
    let coderoom = tmp.path().join(CODEROOM_DIR);
    std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
    std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "x").unwrap();
    // No default_engine anywhere.
    std::fs::write(
        coderoom.join(CONFIG_FILE),
        r#"
host_role = "host"

[roles.host]
"#,
    )
    .unwrap();
    let err = load(tmp.path(), None).expect_err("missing engine must error");
    assert!(matches!(err, ConfigError::MissingDefaultEngine));
}

#[test]
fn merged_always_include_unions_layers_then_filters_never() {
    let user = UserConfig {
        init: Some(InitConfig {
            always_include: vec!["security".into()],
            never_include: vec![],
        }),
        ..UserConfig::default()
    };
    let project = ProjectConfigRaw {
        schema_version: None,
        default_engine: Some(Engine::Cc),
        default_model: None,
        permission_mode: None,
        host_role: "host".into(),
        roles: HashMap::new(),
        init: Some(InitConfig {
            always_include: vec!["data".into()],
            never_include: vec!["security".into()],
        }),
        engines: HashMap::new(),
    };
    let merged = merged_always_include(Some(&user), &project);
    // user's "security" is unioned then vetoed by project's never;
    // project's "data" survives.
    assert_eq!(merged, vec!["data".to_string()]);
}

#[test]
fn schema_version_is_accepted_but_not_required() {
    let tmp = TempDir::new().unwrap();
    let coderoom = tmp.path().join(CODEROOM_DIR);
    std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
    std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "x").unwrap();
    std::fs::write(
        coderoom.join(CONFIG_FILE),
        r#"
schema_version = 1
default_engine = "cc"
host_role = "host"

[roles.host]
"#,
    )
    .unwrap();
    let cfg = load(tmp.path(), None).expect("schema_version = 1 loads");
    assert_eq!(cfg.default_engine, Engine::Cc);
}
