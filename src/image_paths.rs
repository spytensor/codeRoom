//! Image-path parsing and validation for user prompts.
//!
//! CodeRoom's user-facing convention: any `@<path>` token where the
//! path starts with `./`, `/`, or `~/` is an image reference. Other
//! `@<word>` tokens remain role mentions (the role regex requires an
//! alphabetic first char after `@`, so the two syntaxes are disjoint
//! by construction — verified by tests below and by
//! `parse_mentions` in `src/adapter/cc.rs`).
//!
//! Three callers use this module:
//!
//! 1. **REPL `send_and_drain`** — pre-flight validates the user's
//!    typed prompt. A missing / oversized / wrong-format image
//!    aborts the turn entirely with a visible error, so the user
//!    doesn't burn API tokens on garbage.
//! 2. **cc adapter** — extracts the validated refs again at message-
//!    serialize time, reads the file bytes, base64-encodes them, and
//!    appends an `{"type":"image","source":{...}}` content block
//!    alongside the text block.
//! 3. **codex adapter** — extracts paths to build the degradation
//!    note prepended to the prompt text (codex MCP cannot accept
//!    image content blocks; see `docs/proposed-amendments.md`).
//!
//! Gemini does not call this module — its CLI parses `@<path>` in
//! prompt text natively and resolves it via its own `read_file`
//! tool turn.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Max raw bytes per image. Claude's Messages API caps the whole
/// request at 32 MB; base64 inflates ~33% so 20 MB raw keeps us
/// comfortably under the cap even with several images per turn.
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// Max images per turn. Keeps token spend predictable and gives
/// model context room to actually reason about each one.
pub const MAX_IMAGES_PER_TURN: usize = 5;

/// A user-supplied image reference, resolved to an absolute path and
/// classified by media type. Cheap to clone; callers re-derive these
/// from the prompt text at multiple layers (REPL pre-flight, cc
/// adapter serialise, codex adapter degrade note).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// Absolute, canonicalised path on disk.
    pub abs_path: PathBuf,
    /// The token the user typed (e.g. `@./img.png`). Used by adapters
    /// that surface a degradation note so the user sees their own
    /// reference echoed back.
    pub raw_token: String,
    /// IANA media type — `image/png`, `image/jpeg`, etc.
    pub media_type: &'static str,
}

/// Reasons a user's `@<path>` image reference cannot be accepted.
/// Surfaced by the REPL pre-flight before any turn is dispatched, so
/// the user sees the failure inline and the API call isn't wasted.
#[derive(Debug, Error)]
pub enum ImageError {
    /// Path resolves to a file that doesn't exist on disk.
    #[error("image not found: {path}")]
    NotFound {
        /// Absolute path the user's `@<path>` token resolved to.
        path: PathBuf,
    },
    /// File exceeds [`MAX_IMAGE_BYTES`]. Listed in bytes so the user
    /// can see how far over the cap they are.
    #[error("image too large: {path} is {bytes} bytes (limit {limit})")]
    Oversized {
        /// Absolute path to the offending file.
        path: PathBuf,
        /// Raw byte size on disk.
        bytes: u64,
        /// The cap (= [`MAX_IMAGE_BYTES`]).
        limit: u64,
    },
    /// Extension not in the cc adapter's supported set.
    #[error(
        "unsupported image format: {path} (extension `{ext}`; supported: png, jpg, jpeg, gif, webp)"
    )]
    UnsupportedFormat {
        /// Absolute path to the offending file.
        path: PathBuf,
        /// The lowercased extension that triggered the rejection.
        ext: String,
    },
    /// Path has no file extension at all — we can't infer media type.
    #[error("no extension on {path} — supported: png, jpg, jpeg, gif, webp")]
    MissingExtension {
        /// Absolute path to the offending file.
        path: PathBuf,
    },
    /// More than [`MAX_IMAGES_PER_TURN`] images referenced in one
    /// prompt. We reject up front instead of silently truncating so
    /// the user knows their request was incomplete.
    #[error(
        "too many images in one turn ({count}); limit is {limit}. send fewer or split into turns."
    )]
    TooMany {
        /// How many `@<path>` tokens we found in the prompt.
        count: usize,
        /// The cap (= [`MAX_IMAGES_PER_TURN`]).
        limit: usize,
    },
    /// A filesystem error that isn't `NotFound` (permission denied,
    /// I/O error, etc.). Wraps the underlying [`std::io::Error`].
    #[error("could not stat image {path}: {source}")]
    Stat {
        /// Absolute path the stat attempt was made against.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

/// Lossy-but-fast scan: pull out every `@<path>` token without
/// touching disk. Used by adapter degradation notes that want to
/// reference paths the user typed, regardless of whether they're
/// valid. The REPL is responsible for blocking invalid refs *before*
/// dispatch, so callers downstream see only valid paths in practice.
#[must_use]
pub fn extract_path_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        // Find next '@' that's at start-of-string or after whitespace.
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        let at_boundary = i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r');
        if !at_boundary {
            i += 1;
            continue;
        }
        // Need at least one char after `@` that signals a path:
        // `./`, `/`, or `~/`. Anything else (alphabetic) is a role
        // mention or just literal text — bail.
        let rest = &text[i + 1..];
        let is_path_prefix = rest.starts_with("./")
            || rest.starts_with("../")
            || rest.starts_with('/')
            || rest.starts_with("~/");
        if !is_path_prefix {
            i += 1;
            continue;
        }
        // Walk until we hit a whitespace separator — paths in the
        // prompt are space-separated tokens. Quoted paths with
        // embedded spaces are deferred (rare in practice).
        let path_start = i + 1;
        let mut path_end = text.len();
        for (off, ch) in text[path_start..].char_indices() {
            if ch.is_whitespace() {
                path_end = path_start + off;
                break;
            }
        }
        out.push(text[path_start..path_end].to_owned());
        i = path_end;
    }
    out
}

/// Parse + validate every `@<path>` reference in `text`. Resolves
/// relative paths against `cwd` and expands `~` against `home`. The
/// returned vec preserves the order tokens appeared, which matters
/// for cc's content-block ordering — the model sees the same
/// "image 1, image 2" cadence the user wrote.
pub fn parse_image_refs(
    text: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<Vec<ImageRef>, ImageError> {
    let tokens = extract_path_tokens(text);
    if tokens.len() > MAX_IMAGES_PER_TURN {
        return Err(ImageError::TooMany {
            count: tokens.len(),
            limit: MAX_IMAGES_PER_TURN,
        });
    }
    let mut refs = Vec::with_capacity(tokens.len());
    for raw in tokens {
        let expanded = if let Some(rest) = raw.strip_prefix("~/") {
            let Some(home) = home else {
                return Err(ImageError::NotFound {
                    path: PathBuf::from(&raw),
                });
            };
            home.join(rest)
        } else if raw.starts_with('/') {
            PathBuf::from(&raw)
        } else {
            cwd.join(&raw)
        };
        let media_type = media_type_for_path(&expanded)?;
        let metadata = std::fs::metadata(&expanded).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => ImageError::NotFound {
                path: expanded.clone(),
            },
            _ => ImageError::Stat {
                path: expanded.clone(),
                source,
            },
        })?;
        if metadata.len() > MAX_IMAGE_BYTES {
            return Err(ImageError::Oversized {
                path: expanded,
                bytes: metadata.len(),
                limit: MAX_IMAGE_BYTES,
            });
        }
        refs.push(ImageRef {
            abs_path: expanded,
            raw_token: format!("@{raw}"),
            media_type,
        });
    }
    Ok(refs)
}

fn media_type_for_path(path: &Path) -> Result<&'static str, ImageError> {
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return Err(ImageError::MissingExtension {
            path: path.to_path_buf(),
        });
    };
    match ext.to_ascii_lowercase().as_str() {
        "png" => Ok("image/png"),
        "jpg" | "jpeg" => Ok("image/jpeg"),
        "gif" => Ok("image/gif"),
        "webp" => Ok("image/webp"),
        other => Err(ImageError::UnsupportedFormat {
            path: path.to_path_buf(),
            ext: other.to_owned(),
        }),
    }
}

/// Read the file and return a base64-encoded payload, ready to drop
/// into a Messages-API `image` content block's `source.data` field.
/// Only called by the cc adapter; codex and gemini paths never read
/// the file (codex degrades; gemini delegates to its own read_file).
pub fn read_and_encode(image: &ImageRef) -> Result<String, std::io::Error> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let bytes = std::fs::read(&image.abs_path)?;
    Ok(STANDARD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn dummy_png() -> Vec<u8> {
        // Minimum valid PNG file signature + an IHDR chunk. Real bytes
        // never get decoded in tests — only `metadata` + the read for
        // base64 in `read_and_encode` care. 8-byte PNG signature is
        // enough that an extension-and-size-aware caller is happy.
        vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
    }

    #[test]
    fn extract_path_tokens_picks_up_three_prefixes() {
        let tokens = extract_path_tokens("@./a.png @/abs/b.jpg @~/home.gif done");
        assert_eq!(tokens, vec!["./a.png", "/abs/b.jpg", "~/home.gif"]);
    }

    #[test]
    fn extract_path_tokens_ignores_role_mentions() {
        // `@backend` and `@security` are role mentions — alphabetic
        // first char after `@`, no path prefix. Must NOT show up.
        let tokens = extract_path_tokens("@backend take @./x.png to @security");
        assert_eq!(tokens, vec!["./x.png"]);
    }

    #[test]
    fn extract_path_tokens_requires_word_boundary() {
        // An `@` in the middle of an email-like token shouldn't kick
        // off path parsing. (e.g. `foo@./x` — there's no whitespace
        // before `@`, so it's an embedded `@`, not a path mention.)
        let tokens = extract_path_tokens("email me at foo@./x.png");
        assert!(tokens.is_empty());
    }

    #[test]
    fn extract_path_tokens_handles_parent_relative() {
        let tokens = extract_path_tokens("look at @../sibling/img.png");
        assert_eq!(tokens, vec!["../sibling/img.png"]);
    }

    #[test]
    fn parse_image_refs_rejects_missing_files() {
        let tmp = TempDir::new().unwrap();
        let err =
            parse_image_refs("@./does-not-exist.png", tmp.path(), Some(tmp.path())).unwrap_err();
        assert!(matches!(err, ImageError::NotFound { .. }));
    }

    #[test]
    fn parse_image_refs_rejects_unsupported_extension() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("doc.pdf");
        fs::write(&path, b"%PDF-").unwrap();
        let err = parse_image_refs("@./doc.pdf", tmp.path(), Some(tmp.path())).unwrap_err();
        assert!(matches!(err, ImageError::UnsupportedFormat { ext, .. } if ext == "pdf"));
    }

    #[test]
    fn parse_image_refs_rejects_oversize() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("huge.png");
        // 21 MB — sparse write so the test stays fast.
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        let err = parse_image_refs("@./huge.png", tmp.path(), Some(tmp.path())).unwrap_err();
        assert!(matches!(err, ImageError::Oversized { bytes, .. } if bytes > MAX_IMAGE_BYTES));
    }

    #[test]
    fn parse_image_refs_rejects_too_many() {
        let tmp = TempDir::new().unwrap();
        // Build a prompt with N + 1 paths; even though none exist on
        // disk, the count check fires before file stat.
        let n = MAX_IMAGES_PER_TURN + 1;
        let prompt = (0..n)
            .map(|i| format!("@./img{i}.png"))
            .collect::<Vec<_>>()
            .join(" ");
        let err = parse_image_refs(&prompt, tmp.path(), Some(tmp.path())).unwrap_err();
        assert!(matches!(err, ImageError::TooMany { count, .. } if count == n));
    }

    #[test]
    fn parse_image_refs_resolves_relative_against_cwd() {
        let tmp = TempDir::new().unwrap();
        let img = tmp.path().join("ok.png");
        fs::write(&img, dummy_png()).unwrap();
        let refs = parse_image_refs("@./ok.png", tmp.path(), None).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].abs_path, img);
        assert_eq!(refs[0].media_type, "image/png");
        assert_eq!(refs[0].raw_token, "@./ok.png");
    }

    #[test]
    fn parse_image_refs_expands_tilde_against_home() {
        let tmp = TempDir::new().unwrap();
        let img = tmp.path().join("home.png");
        fs::write(&img, dummy_png()).unwrap();
        // home is `tmp.path()`, raw is `@~/home.png` → tmp/home.png.
        let refs = parse_image_refs("@~/home.png", Path::new("/unused"), Some(tmp.path())).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].abs_path, img);
    }

    #[test]
    fn parse_image_refs_accepts_supported_formats() {
        let tmp = TempDir::new().unwrap();
        for (name, expected_mime) in [
            ("a.png", "image/png"),
            ("b.jpg", "image/jpeg"),
            ("c.jpeg", "image/jpeg"),
            ("d.gif", "image/gif"),
            ("e.webp", "image/webp"),
        ] {
            fs::write(tmp.path().join(name), dummy_png()).unwrap();
            let refs = parse_image_refs(&format!("@./{name}"), tmp.path(), None).unwrap();
            assert_eq!(refs.len(), 1, "{name} should produce one ref");
            assert_eq!(refs[0].media_type, expected_mime);
        }
    }

    #[test]
    fn read_and_encode_round_trips_a_known_payload() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("z.png");
        let payload = dummy_png();
        fs::write(&path, &payload).unwrap();
        let img = ImageRef {
            abs_path: path,
            raw_token: "@./z.png".into(),
            media_type: "image/png",
        };
        let b64 = read_and_encode(&img).unwrap();
        assert_eq!(STANDARD.decode(b64).unwrap(), payload);
    }
}
