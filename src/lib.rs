//! CodeRoom — a coordination shell for multi-role agent CLI sessions.
//!
//! Public API surface is intentionally small at v0.x; the binary `cr` is the
//! primary consumer. See `docs/architecture.md` for the v0.1 constitution and
//! `docs/v0.2-trust-and-interrupt.md` for the v0.2 amendment.

#![doc(html_root_url = "https://docs.rs/coderoom/0.4.1")]
#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod adapter;
pub mod bus;
pub mod config;
pub mod config_cmd;
pub mod config_layered;
pub mod cost;
pub mod crep;
pub mod detect;
pub mod doctor;
pub mod engines;
pub mod image_paths;
pub mod init;
pub mod output;
pub(crate) mod peer_quote;
pub mod permissions;
pub mod pointers;
pub mod priors;
pub mod prompt_cmd;
pub mod repl;
pub mod role;
pub mod turn;
pub mod update;
mod work;
