//! CodeRoom — a coordination shell for multi-role agent CLI sessions.
//!
//! Public API surface is intentionally small at v0.1; the binary `cr` is the
//! primary consumer. See `docs/architecture.md` for the design constitution.

#![doc(html_root_url = "https://docs.rs/coderoom/0.1.3")]
#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod adapter;
pub mod bus;
pub mod config;
pub mod cost;
pub mod crep;
pub mod detect;
pub mod init;
pub mod priors;
pub mod repl;
pub mod role;
