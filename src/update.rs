//! `cr update` — best-effort upgrade of the local `cr` binary.
//!
//! v0.2 strategy: detect how the binary was installed by inspecting
//! the path of the running executable, then dispatch:
//!
//! - **npm install** (the supported path): shell out to
//!   `npm install -g @spytensor/coderoom@latest`. The npm package's
//!   postinstall script will pull the right platform binary from the
//!   GitHub release and verify its SHA-256.
//! - **Anything else** (raw GitHub binary, `cargo install`, hand-built
//!   from source): print clear instructions for the matching method
//!   instead of silently corrupting the install.
//!
//! No HTTP deps, no caching, no background task — just dispatch on
//! observable state and print useful output. The once-per-day update
//! notifier is intentionally deferred: it'd add an HTTP layer plus a
//! cache file just to save users one `cr update` per week, and the
//! design tradeoffs deserve their own PR.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// npm package name. Bumping this requires re-running the npm publish
/// flow and updating the README.
const NPM_PACKAGE: &str = "@spytensor/coderoom";

/// How the `cr` binary appears to have been installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallSource {
    /// Installed via `npm install -g @spytensor/coderoom`. We can
    /// re-run the install to upgrade.
    Npm {
        /// Resolved path to the running binary (for diagnostics).
        binary: PathBuf,
    },
    /// Installed via `cargo install coderoom` — currently disabled
    /// (`publish = false`) but reserved for when crates.io publish
    /// flips on.
    Cargo {
        /// Resolved path to the running binary.
        binary: PathBuf,
    },
    /// Anything else — manual download of a release binary, hand-
    /// built from source, packaged by Homebrew, etc. We don't try
    /// to update these.
    Unknown {
        /// Resolved path to the running binary.
        binary: PathBuf,
    },
}

/// Public entry point — `cr update`.
pub fn run() -> Result<()> {
    let exe = std::env::current_exe().context("locating the running `cr` binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let source = classify(&exe);
    dispatch(&source)
}

fn dispatch(source: &InstallSource) -> Result<()> {
    match source {
        InstallSource::Npm { binary } => {
            println!("Detected npm install at {}", binary.display());
            println!("Running: npm install -g {NPM_PACKAGE}@latest");
            println!();
            let status = Command::new("npm")
                .args(["install", "-g", &format!("{NPM_PACKAGE}@latest")])
                .status()
                .context("launching `npm`. Is it on $PATH?")?;
            if !status.success() {
                bail!("npm install failed with status {status}");
            }
            println!();
            println!("✓ updated. run `cr --version` to confirm.");
            Ok(())
        }
        InstallSource::Cargo { binary } => {
            println!("Detected cargo install at {}", binary.display());
            println!();
            println!("CodeRoom is not yet published on crates.io (v0.2).");
            println!("To upgrade, reinstall via npm (recommended) or rebuild from source:");
            println!();
            println!("  npm install -g {NPM_PACKAGE}@latest    # recommended");
            println!("  cargo install --git https://github.com/spytensor/codeRoom --force");
            Ok(())
        }
        InstallSource::Unknown { binary } => {
            println!("Could not auto-detect how `cr` was installed.");
            println!("Running binary: {}", binary.display());
            println!();
            println!("To upgrade, use whichever method you originally installed with:");
            println!();
            println!("  npm install -g {NPM_PACKAGE}@latest    # recommended");
            println!("  cargo install --git https://github.com/spytensor/codeRoom --force");
            println!();
            println!("Or grab a fresh binary from:");
            println!("  https://github.com/spytensor/codeRoom/releases/latest");
            Ok(())
        }
    }
}

/// Classify an absolute binary path into an [`InstallSource`].
///
/// Heuristics (cheap, no IO beyond what's in the path itself):
///
/// - Path component contains `node_modules` AND somewhere upstream is
///   `@spytensor/coderoom` → npm.
/// - Path is under `$CARGO_HOME/bin` (or `~/.cargo/bin` if `$CARGO_HOME`
///   is unset) → cargo.
/// - Otherwise → unknown.
#[must_use]
pub fn classify(binary: &Path) -> InstallSource {
    if is_npm_path(binary) {
        return InstallSource::Npm {
            binary: binary.to_path_buf(),
        };
    }
    if is_cargo_path(binary) {
        return InstallSource::Cargo {
            binary: binary.to_path_buf(),
        };
    }
    InstallSource::Unknown {
        binary: binary.to_path_buf(),
    }
}

fn is_npm_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    // Cover the two common layouts: per-package install
    // (`.../node_modules/@spytensor/coderoom/bin/cr`) and the
    // bin-shimmed global install (`.../bin/cr` symlinking the above
    // — but `current_exe()` resolves the symlink so we typically see
    // the per-package layout after canonicalize()).
    s.contains("node_modules") && s.contains("@spytensor/coderoom")
}

fn is_cargo_path(p: &Path) -> bool {
    if let Ok(cargo_home) = std::env::var("CARGO_HOME") {
        if p.starts_with(&cargo_home) {
            return true;
        }
    }
    if let Some(home) = dirs::home_dir() {
        if p.starts_with(home.join(".cargo")) {
            return true;
        }
    }
    false
}

// ---- Tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_npm_layout() {
        // Typical canonicalized npm-global path on linux:
        let p = Path::new(
            "/home/me/.nvm/versions/node/v20.10.0/lib/node_modules/@spytensor/coderoom/bin/cr",
        );
        assert!(matches!(classify(p), InstallSource::Npm { .. }));
    }

    #[test]
    fn classify_macos_npm_layout() {
        let p = Path::new("/usr/local/lib/node_modules/@spytensor/coderoom/bin/cr");
        assert!(matches!(classify(p), InstallSource::Npm { .. }));
    }

    #[test]
    fn classify_cargo_install_via_home() {
        // Use the actual home dir so the test passes wherever it runs.
        let home = dirs::home_dir().expect("test env has a home dir");
        let p = home.join(".cargo/bin/cr");
        assert!(matches!(classify(&p), InstallSource::Cargo { .. }));
    }

    #[test]
    fn classify_arbitrary_path_is_unknown() {
        let p = Path::new("/opt/coderoom/cr");
        assert!(matches!(classify(p), InstallSource::Unknown { .. }));
    }

    #[test]
    fn classify_node_modules_without_our_package_is_unknown() {
        // Some stranger's node_modules dir — not our package.
        let p = Path::new("/opt/somethingelse/node_modules/foo/bar/cr");
        assert!(matches!(classify(p), InstallSource::Unknown { .. }));
    }
}
