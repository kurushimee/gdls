//! `gd_server` — the gdls LSP server: lifecycle, VFS, position mapping, query services, and the
//! per-file diagnostics engine. Produces the `gdls` binary.
//!
//! The server speaks LSP/JSON-RPC over stdio to Claude Code, with **no Godot process at runtime**.
//! This crate is split into a library (so the event loop can be driven over an in-memory connection
//! in integration tests) and a thin `main.rs` binary that calls [`run`].

pub mod api_dump;
pub mod bench;
pub mod cancellation;
pub mod config;
pub mod memory;
pub mod observability;
pub mod position;
pub mod uri;
pub mod vfs;
pub mod watcher;
pub mod workspace;
pub mod xfile;

mod handlers;
pub mod logging;
mod native_render;
mod server;

pub use server::{run, run_with_recorder, serve, serve_with_injected_watcher, serve_with_recorder};
pub use workspace::Workspace;
