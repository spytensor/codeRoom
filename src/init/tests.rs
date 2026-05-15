use super::*;
use crate::adapter::PermissionMode;
use crate::config::{Config, RoleEntry};
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use tempfile::TempDir;

/// Visible-cell width of `s`, ignoring ANSI SGR escapes. ASCII-only
/// approximation — matches the role picker's text content.
fn visible_width(s: &str) -> usize {
    let mut count = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            // Skip CSI ... letter
            chars.next();
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        count += 1;
    }
    count
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn ctrl_c_key_aborts_raw_mode_wizard() {
    let event = Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    ));

    assert_eq!(wizard_key_from_event(&event), Some(WizardKey::Abort));
}

fn snapshot_scan() -> detect::ProjectScan {
    detect::ProjectScan {
        root: PathBuf::from("/repo/codeRoom"),
        stack: vec![
            detect::StackSignal::CargoToml,
            detect::StackSignal::GithubWorkflows,
            detect::StackSignal::ExistingClaudeMd { line_count: 42 },
        ],
        suggested_roles: vec!["host", "backend", "security", "ci"],
    }
}

fn snapshot_plan() -> Vec<RolePlan> {
    vec![
        RolePlan {
            name: "host".into(),
            engine: Engine::Cc,
        },
        RolePlan {
            name: "backend".into(),
            engine: Engine::Cc,
        },
        RolePlan {
            name: "security".into(),
            engine: Engine::Codex,
        },
    ]
}

fn sample_choices() -> Vec<RoleChoice> {
    ROLE_CATALOG
        .iter()
        .map(|info| RoleChoice {
            info: *info,
            selected: matches!(info.name, "host" | "backend" | "security"),
        })
        .collect()
}

#[test]
fn snapshot_init_role_picker() {
    let scan = snapshot_scan();
    let rendered = strip_ansi(&render_role_picker(
        Path::new("/repo/codeRoom"),
        &scan,
        &sample_choices(),
        1,
    ));
    insta::assert_snapshot!(rendered, @r"
pick roles · setting up coderoom in codeRoom
space toggles · ↑↓ moves · enter continues · esc backs out

detected: Cargo.toml (Rust) · .github/workflows/ · CLAUDE.md (42 lines)

    [x] ● @host        orchestrates requests and keeps the room coherent · req…
  > [x] ● @backend     APIs, services, storage boundaries
    [ ] ● @frontend    UI, components, routing, client-side state
    [x] ● @security    authn, authz, threat modeling
    [ ] ● @data        schemas, migrations, query patterns
    [ ] ● @devops      CI/CD, infra, deploys, runtime health
    [ ] ● @ci          workflows, checks, release gates
    [ ] ● @qa          test strategy, edge cases, regression risk
    [ ] ● @docs        technical writing, examples, API reference

3 selected · host is always present · enter continues
");
}

#[test]
fn snapshot_init_role_expansion_picker() {
    let scan = snapshot_scan();
    let rendered = strip_ansi(&render_role_expansion_picker(
        Path::new("/repo/codeRoom"),
        &scan,
        &sample_choices(),
        2,
    ));
    insta::assert_snapshot!(rendered, @r"
suggest roles · setting up coderoom in codeRoom
space toggles · ↑↓ moves · enter adds selected · esc skips

detected: Cargo.toml (Rust) · .github/workflows/ · CLAUDE.md (42 lines)
CodeRoom found only @host. Choose the specialists to add:

    [x] ● @host        orchestrates requests and keeps the room coherent · exi…
    [x] ● @backend     APIs, services, storage boundaries
  > [ ] ● @frontend    UI, components, routing, client-side state
    [x] ● @security    authn, authz, threat modeling
    [ ] ● @data        schemas, migrations, query patterns
    [ ] ● @devops      CI/CD, infra, deploys, runtime health
    [ ] ● @ci          workflows, checks, release gates
    [ ] ● @qa          test strategy, edge cases, regression risk
    [ ] ● @docs        technical writing, examples, API reference

2 new role(s) selected · enter writes config and priors
");
}

#[test]
fn snapshot_init_engine_picker() {
    let installed = InstalledEngines {
        cc: true,
        codex: true,
        gemini: false,
    };
    let roles = vec!["host".into(), "backend".into(), "security".into()];
    let assignments = HashMap::from([
        ("host".into(), Engine::Cc),
        ("backend".into(), Engine::Cc),
        ("security".into(), Engine::Codex),
    ]);
    let rendered = strip_ansi(&render_engine_picker(
        Path::new("/repo/codeRoom"),
        &installed,
        &roles,
        &assignments,
        2,
    ));
    insta::assert_snapshot!(rendered, @r"
assign engines · setting up coderoom in codeRoom
↑/↓ moves · ←/→ cycles engine · enter continues · esc goes back

detected on your system:
  ✓ claude-code   installed
  ✓ codex         installed
  ✗ gemini-cli    not installed · github.com/google/gemini-cli

  role          ‹ engine     › model              note
    @host         ‹ claude-code › claude default     ready
    @backend      ‹ claude-code › claude default     ready
  > @security     ‹ codex      › codex default      ready

defaults are editable later in .coderoom/config.toml
");
}

#[test]
fn snapshot_init_confirm() {
    let scan = snapshot_scan();
    let rendered = strip_ansi(&render_confirm(
        Path::new("/repo/codeRoom"),
        &scan,
        &snapshot_plan(),
    ));
    insta::assert_snapshot!(rendered, @r"
ready to write · setting up coderoom in codeRoom
nothing is written until Enter

will create:

.coderoom/
├─ config.toml              3 roles
├─ shared.md                project-wide priors
├─ roles/
│  ├─ host.md            claude-code
│  ├─ backend.md         claude-code
│  └─ security.md        codex
└─ .gitignore

  role           engine       focus
  @host          claude-code  orchestrates requests and keeps the room coherent
  @backend       claude-code  APIs, services, storage boundaries
  @security      codex        authn, authz, threat modeling

! found existing CLAUDE.md (42 lines).
  coderoom will not touch it; split assistance can land separately.

enter writes · esc goes back · q aborts
");
}

#[test]
fn picker_row_never_exceeds_terminal_columns_at_60() {
    let info = role_info("backend");
    let row = picker_row(&info, true, true, 60, None);
    assert!(
        visible_width(&row) <= 60,
        "row visible width = {}, columns = 60, row = {row:?}",
        visible_width(&row)
    );
}

#[test]
fn picker_row_never_exceeds_terminal_columns_at_80() {
    for info in ROLE_CATALOG {
        for selected in [true, false] {
            for is_cursor in [true, false] {
                for tag in [None, Some("required"), Some("existing")] {
                    let row = picker_row(info, selected, is_cursor, 80, tag);
                    assert!(
                        visible_width(&row) <= 80,
                        "row visible width = {}, columns = 80, row = {row:?}",
                        visible_width(&row)
                    );
                }
            }
        }
    }
}

#[test]
fn picker_row_uses_more_room_at_120() {
    // At wider widths the description should not be truncated for
    // any of the catalog entries (their descriptions all fit).
    for info in ROLE_CATALOG {
        let row = picker_row(info, true, false, 120, None);
        assert!(
            !row.contains('…'),
            "120-col row should not be truncated, got {row:?}",
        );
    }
}

#[test]
fn picker_row_handles_extreme_narrow_columns_without_panic() {
    // Below the floor (40 effective) we still produce output; the
    // description is heavily truncated but the row stays one line.
    let info = role_info("frontend");
    let _row = picker_row(&info, true, false, 30, None);
    let _row = picker_row(&info, true, false, 0, None);
}

/// Visual smoke. Run with:
///   cargo test --lib picker_visual_smoke -- --nocapture --ignored
/// and eyeball the three rendered widths. Not a real test — it's a
/// substitute for "open three terminals at 60/80/120 cols and try".
#[test]
#[ignore = "visual-only; render a sample picker at 60/80/120 cols for human review"]
fn picker_visual_smoke() {
    for width in [60usize, 80, 120] {
        eprintln!("\n──── picker at columns = {width} ────");
        for (i, info) in ROLE_CATALOG.iter().enumerate() {
            let selected = matches!(info.name, "host" | "backend" | "security");
            let is_cursor = i == 1;
            let tag = if info.name == "host" {
                Some("existing")
            } else {
                None
            };
            eprintln!("{}", picker_row(info, selected, is_cursor, width, tag));
        }
    }
}

#[test]
fn full_role_expansion_picker_fits_at_80_columns() {
    let dir = TempDir::new().unwrap();
    let scan = detect::scan(dir.path());
    let choices = sample_choices();
    // We can't override picker_columns() at the call site, so render
    // a single row at columns = 80 across the catalog and verify
    // none would exceed terminal width — the assemblage of rows in
    // render_role_expansion_picker shares the same width budget.
    for choice in &choices {
        let row = picker_row(&choice.info, choice.selected, false, 80, None);
        assert!(visible_width(&row) <= 80);
    }
    // Header / scan / footer lines come from push_header etc. They
    // are short by construction; only the rows hit the width gate.
    let _ = scan; // kept to anchor the project-scan codepath
}

fn host_only_config(default_engine: Engine, default_model: Option<&str>) -> Config {
    Config {
        default_engine,
        default_model: default_model.map(ToOwned::to_owned),
        permission_mode: PermissionMode::Ask,
        host_role: "host".into(),
        roles: HashMap::from([("host".into(), RoleEntry::default())]),
    }
}

#[test]
fn init_yes_creates_minimal_valid_layout() {
    let tmp = TempDir::new().unwrap();
    run(tmp.path(), InitOptions::auto()).expect("auto init succeeds in fresh dir");

    let coderoom = tmp.path().join(CODEROOM_DIR);
    assert!(coderoom.is_dir());
    assert!(coderoom.join(CONFIG_FILE).is_file());
    assert!(coderoom.join("shared.md").is_file());
    assert!(coderoom.join(ROLES_DIR).join("host.md").is_file());
    assert!(coderoom
        .join(crate::gate::GATE_TEMPLATES_DIR)
        .join("code-review-gate.md")
        .is_file());
    assert!(coderoom.join(".gitignore").is_file());
}

#[test]
fn init_yes_output_passes_config_validation() {
    let tmp = TempDir::new().unwrap();
    run(tmp.path(), InitOptions::auto()).expect("init");
    let cfg = Config::load_test(tmp.path()).expect("init output should be a valid config");
    assert_eq!(cfg.host_role, "host");
    assert!(cfg.is_host("host"));
}

#[test]
fn init_refuses_to_overwrite_existing_dir() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(CODEROOM_DIR)).unwrap();
    let err = run(tmp.path(), InitOptions::auto()).expect_err("should refuse to overwrite");
    assert!(err.to_string().contains("already exists"), "got: {err}");
}

#[test]
fn default_host_only_config_is_expandable() {
    let cfg = host_only_config(Engine::Cc, None);
    assert!(is_default_host_only(&cfg));

    let mut cfg_with_backend = cfg.clone();
    cfg_with_backend
        .roles
        .insert("backend".into(), RoleEntry::default());
    assert!(!is_default_host_only(&cfg_with_backend));
}

#[test]
fn expansion_defaults_keep_model_engine_pair_safe() {
    let cfg = host_only_config(Engine::Cc, Some("opus"));
    let installed = InstalledEngines {
        cc: true,
        codex: true,
        gemini: false,
    };

    assert_eq!(
        expansion_engine_for_role("security", &cfg, &installed),
        Engine::Cc
    );
    let additions = role_additions_from_plan(
        &[RolePlan {
            name: "security".into(),
            engine: Engine::Codex,
        }],
        &cfg,
    );
    assert_eq!(additions[0].engine, None);
}

#[test]
fn expansion_uses_codex_for_security_when_no_default_model_can_leak() {
    let cfg = host_only_config(Engine::Cc, None);
    let installed = InstalledEngines {
        cc: true,
        codex: true,
        gemini: false,
    };
    let engine = expansion_engine_for_role("security", &cfg, &installed);
    assert_eq!(engine, Engine::Codex);

    let additions = role_additions_from_plan(
        &[RolePlan {
            name: "security".into(),
            engine,
        }],
        &cfg,
    );
    assert_eq!(additions[0].engine, Some(Engine::Codex));
    assert_eq!(additions[0].model, None);
}

#[test]
fn default_priors_templates_stay_compact() {
    assert!(word_count(DEFAULT_HOST_PRIORS) <= 160);
    assert!(word_count(DEFAULT_ROLE_TEMPLATE) <= 200);
    assert!(word_count(DEFAULT_SHARED_PRIORS) <= 80);
    for forbidden in ["@name", "From @", "/patch", "/journal", "cr-task"] {
        assert!(
            !DEFAULT_SHARED_PRIORS.contains(forbidden),
            "shared priors should not carry kernel protocol marker {forbidden}"
        );
    }
    assert!(DEFAULT_SHARED_PRIORS.contains("Team-wide priors"));
    assert!(DEFAULT_SHARED_PRIORS.contains("project standards"));
    for required in [
        "@host",
        "specialist",
        "peer-quote",
        "From @role",
        "current-thread evidence",
        "@role turn",
    ] {
        assert!(
            DEFAULT_HOST_PRIORS.contains(required),
            "host priors should explain {required}"
        );
    }
    for required in [
        "{ROLE}",
        "{HOST}",
        "{PEERS}",
        "peer-quote",
        "From @role",
        "current-thread evidence",
        "@role turn",
    ] {
        assert!(
            DEFAULT_ROLE_TEMPLATE.contains(required),
            "role template should contain {required}"
        );
    }
}

#[test]
fn detected_stack_creates_extra_roles_in_config() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
    run(tmp.path(), InitOptions::auto()).expect("init");

    let cfg = Config::load_test(tmp.path()).expect("valid config");
    // Cargo.toml → host + backend + security
    assert!(cfg.roles.contains_key("host"));
    assert!(cfg.roles.contains_key("backend"));
    assert!(cfg.roles.contains_key("security"));

    let coderoom = tmp.path().join(CODEROOM_DIR);
    assert!(coderoom.join(ROLES_DIR).join("backend.md").is_file());
    assert!(coderoom.join(ROLES_DIR).join("security.md").is_file());
}

#[test]
fn role_template_substitutes_role_name() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("go.mod"), "module x\n").unwrap();
    run(tmp.path(), InitOptions::auto()).expect("init");

    let backend_priors = std::fs::read_to_string(
        tmp.path()
            .join(CODEROOM_DIR)
            .join(ROLES_DIR)
            .join("backend.md"),
    )
    .unwrap();
    // Template's `{ROLE}` placeholder should be replaced.
    assert!(!backend_priors.contains("{ROLE}"));
    assert!(!backend_priors.contains("{HOST}"));
    assert!(!backend_priors.contains("{PEERS}"));
    assert!(backend_priors.contains("@host"));
    assert!(backend_priors.contains("@backend"));
}

#[test]
fn planned_files_lists_in_render_order() {
    let coderoom = PathBuf::from("/tmp/p/.coderoom");
    let paths = planned_files(
        &coderoom,
        &[
            RolePlan {
                name: "host".into(),
                engine: Engine::Cc,
            },
            RolePlan {
                name: "backend".into(),
                engine: Engine::Cc,
            },
        ],
    );
    let display: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    assert_eq!(
        display,
        vec![
            "/tmp/p/.coderoom/config.toml",
            "/tmp/p/.coderoom/shared.md",
            "/tmp/p/.coderoom/roles/host.md",
            "/tmp/p/.coderoom/roles/backend.md",
            "/tmp/p/.coderoom/gate-templates/tier-classify.md",
            "/tmp/p/.coderoom/gate-templates/research-gate.md",
            "/tmp/p/.coderoom/gate-templates/plan-gate.md",
            "/tmp/p/.coderoom/gate-templates/plan-review-gate.md",
            "/tmp/p/.coderoom/gate-templates/code-review-gate.md",
            "/tmp/p/.coderoom/gate-templates/precommit-gate.md",
            "/tmp/p/.coderoom/gate-templates/signoff-gate.md",
            "/tmp/p/.coderoom/.gitignore",
        ]
    );
}

fn word_count(input: &str) -> usize {
    input.split_whitespace().count()
}
