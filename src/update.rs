//! `cr update` (check) and `cr upgrade` (install) — two commands, not one.
//!
//! Why split. Up through v0.1.8 there was a single `cr update` that
//! shelled out to `npm install -g …@latest` and printed `✓ updated`
//! whenever npm exited 0. That's a lie when npm's tarball cache is
//! stale: the registry pointer says the new version is available, but
//! npm extracts the cached old tarball and returns success. Users see
//! `✓ updated` but `cr --version` is unchanged. This shipped real
//! breakage: 0.1.7 → 0.1.8 left users on 0.1.7 with the broken role
//! picker.
//!
//! New shape, brew-style:
//!
//! - `cr update` is read-only. Asks the registry what `@latest` is,
//!   compares to `env!("CARGO_PKG_VERSION")`, prints the diff. No
//!   side effects, safe to run anywhere.
//! - `cr upgrade` actually installs. After `npm install -g` returns,
//!   it re-execs the binary at `current_exe()` and parses
//!   `--version` to confirm the bytes on disk really changed. If
//!   the post-install version still equals the pre-install version
//!   AND the registry has a newer one, that's the cache-stale case;
//!   we print the exact remediation (`npm cache clean --force &&
//!   cr upgrade`) instead of claiming success.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::output;
use crossterm::style::Stylize;

/// npm package name. Bumping this requires re-running the npm publish
/// flow and updating the README.
const NPM_PACKAGE: &str = "@spytensor/coderoom";

/// Compile-time version of the running binary. We compare against this
/// to detect whether `npm install -g` produced any real change.
const RUNNING_VERSION: &str = env!("CARGO_PKG_VERSION");

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

/// `cr update` — check the registry, print the diff, do nothing else.
pub fn check() -> Result<()> {
    let current = RUNNING_VERSION;
    println!("local:    cr {current}");

    let latest = match query_npm_latest() {
        Ok(v) => v,
        Err(error) => {
            output::warn(format!("could not reach the npm registry: {error}"));
            output::hint("network down? proxy? try again later, or run `cr upgrade --force`.");
            return Ok(());
        }
    };
    println!("registry: cr {latest}");
    println!();

    if latest == current {
        output::ok(format!("cr is up to date ({current})."));
    } else {
        let upgrade_cmd = "cr upgrade".to_owned();
        println!(
            "{} {} {} {}",
            "→".with(output::INFO),
            format!("cr {current}").with(output::TEXT),
            "→".with(output::FADE),
            format!("cr {latest}").with(output::EM).bold(),
        );
        println!();
        println!(
            "run {} to install the new version.",
            upgrade_cmd.with(output::KEY).bold()
        );
    }
    Ok(())
}

/// `cr upgrade` — actually install the latest version, with verification.
pub fn upgrade() -> Result<()> {
    let exe = std::env::current_exe().context("locating the running `cr` binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let source = classify(&exe);

    match &source {
        InstallSource::Cargo { binary } => {
            print_cargo_instructions(binary);
            return Ok(());
        }
        InstallSource::Unknown { binary } => {
            print_unknown_instructions(binary);
            return Ok(());
        }
        InstallSource::Npm { .. } => {} // continue
    }

    // Pre-flight: query the registry. If we already have the latest, skip
    // the install entirely — saves users the npm round-trip and the
    // anxious "did anything happen?" experience.
    let latest = match query_npm_latest() {
        Ok(v) => v,
        Err(error) => {
            output::warn(format!("could not reach the npm registry: {error}"));
            output::hint("upgrade aborted. check your network and try again.");
            return Ok(());
        }
    };

    if latest == RUNNING_VERSION {
        output::ok(format!("already on the latest version ({latest})."));
        return Ok(());
    }

    println!(
        "upgrading {} → {}",
        format!("cr {RUNNING_VERSION}").with(output::TEXT),
        format!("cr {latest}").with(output::EM).bold(),
    );
    println!(
        "{} {}",
        "running:".with(output::DIM),
        format!("npm install -g {NPM_PACKAGE}@latest").with(output::KEY),
    );
    println!();

    let status = Command::new("npm")
        .args(["install", "-g", &format!("{NPM_PACKAGE}@latest")])
        .status()
        .context("launching `npm`. Is it on $PATH?")?;
    if !status.success() {
        bail!("npm install failed with status {status}");
    }
    println!();

    // Verify the binary on disk actually changed. `current_exe()` is
    // still the path npm overwrote; running it now invokes the new
    // bytes, not us.
    let post_version = match read_binary_version(&exe) {
        Ok(v) => v,
        Err(error) => {
            output::warn(format!("could not verify the new binary version: {error}"));
            output::hint("install completed, but `cr --version` did not respond as expected.");
            return Ok(());
        }
    };

    if post_version == latest {
        output::ok(format!("upgraded to cr {post_version}."));
    } else if post_version == RUNNING_VERSION {
        // Cache-stale: npm exited 0 but the bytes on disk are unchanged.
        output::bad("upgrade did not take effect.");
        output::hint(format!(
            "registry latest is {latest}, but the binary is still {post_version}.",
        ));
        output::hint("npm likely served a stale tarball from its cache.");
        println!();
        println!("{}", "fix it with:".with(output::TEXT));
        println!(
            "  {}",
            "npm cache clean --force && cr upgrade"
                .with(output::KEY)
                .bold(),
        );
        bail!("upgrade verification failed: binary still reports {post_version}");
    } else {
        // Some other surprise — registry says X, npm installed Y.
        output::warn(format!(
            "upgraded to cr {post_version}, but registry latest is {latest}.",
        ));
        output::hint("you may be on a beta tag, or another shell layered an override.");
    }
    Ok(())
}

fn print_cargo_instructions(binary: &Path) {
    println!("Detected cargo install at {}", binary.display());
    println!();
    println!("CodeRoom is not yet published on crates.io (v0.2).");
    println!("To upgrade, reinstall via npm (recommended) or rebuild from source:");
    println!();
    println!("  npm install -g {NPM_PACKAGE}@latest    # recommended");
    println!("  cargo install --git https://github.com/spytensor/codeRoom --force");
}

fn print_unknown_instructions(binary: &Path) {
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
}

/// Ask npm for the registry's `latest` dist-tag. Pure read; no install.
fn query_npm_latest() -> Result<String> {
    let output = Command::new("npm")
        .args(["view", NPM_PACKAGE, "version", "--json"])
        .output()
        .context("launching `npm view`. Is npm on $PATH?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("npm view failed: {}", stderr.trim());
    }
    parse_npm_view_version(&String::from_utf8_lossy(&output.stdout))
}

/// `npm view <pkg> version --json` returns either `"0.1.8"` (a JSON
/// string) when one version exists, or a JSON array of strings; we
/// only care about the simple-string case the registry returns for a
/// single-version query against latest.
fn parse_npm_view_version(stdout: &str) -> Result<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        bail!("npm view returned no output");
    }
    // Strip wrapping quotes if present.
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(trimmed);
    if unquoted.is_empty() || unquoted.contains(',') || unquoted.contains('[') {
        bail!("unexpected npm view output: {trimmed}");
    }
    Ok(unquoted.to_owned())
}

/// Run `<binary> --version` and pull out the semver part. The `cr`
/// binary prints `cr 0.1.8` via clap; we accept either that exact
/// shape or any token immediately following the binary name.
fn read_binary_version(binary: &Path) -> Result<String> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .context("running the freshly installed binary")?;
    if !output.status.success() {
        bail!("binary --version exited non-zero");
    }
    parse_version_output(&String::from_utf8_lossy(&output.stdout))
}

fn parse_version_output(stdout: &str) -> Result<String> {
    let line = stdout
        .lines()
        .next()
        .context("--version produced no output")?;
    // "cr 0.1.8" → "0.1.8". Take the last whitespace-separated token
    // so layout tweaks (e.g. "coderoom (cr) 0.1.8") still parse.
    let token = line
        .split_whitespace()
        .last()
        .context("empty --version line")?;
    if !token.chars().any(|c| c == '.') {
        bail!("could not find a version token in: {line}");
    }
    Ok(token.to_owned())
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
        let p = Path::new("/opt/somethingelse/node_modules/foo/bar/cr");
        assert!(matches!(classify(p), InstallSource::Unknown { .. }));
    }

    #[test]
    fn parse_npm_view_handles_quoted_string() {
        // `npm view ... version --json` returns a JSON string.
        assert_eq!(parse_npm_view_version("\"0.1.8\"\n").unwrap(), "0.1.8");
        assert_eq!(parse_npm_view_version("\"0.1.8\"").unwrap(), "0.1.8");
    }

    #[test]
    fn parse_npm_view_handles_unquoted_string() {
        // Without --json the output is a bare string; we accept either.
        assert_eq!(parse_npm_view_version("0.1.8\n").unwrap(), "0.1.8");
    }

    #[test]
    fn parse_npm_view_rejects_array() {
        // `npm view <pkg> versions` would be a JSON array — not what we
        // asked for, but defend against it.
        assert!(parse_npm_view_version("[\"0.1.7\",\"0.1.8\"]").is_err());
    }

    #[test]
    fn parse_npm_view_rejects_empty() {
        assert!(parse_npm_view_version("").is_err());
        assert!(parse_npm_view_version("\n").is_err());
    }

    #[test]
    fn parse_version_output_handles_clap_default_shape() {
        // clap's default `--version` output for the `cr` binary.
        assert_eq!(parse_version_output("cr 0.1.9\n").unwrap(), "0.1.9");
    }

    #[test]
    fn parse_version_output_handles_extra_tokens() {
        // Defend against future shape changes like "coderoom (cr) 0.1.9".
        assert_eq!(
            parse_version_output("coderoom (cr) 0.1.9\n").unwrap(),
            "0.1.9"
        );
    }

    #[test]
    fn parse_version_output_rejects_no_dot() {
        // Anything without a dot can't be a semver — fail cleanly.
        assert!(parse_version_output("cr unreleased\n").is_err());
    }
}
