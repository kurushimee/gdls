//! The one integration-test binary for `gd_project`. Every `tests/*.rs` file is a module of it,
//! so the crate links a single test executable and all of its tests run in one parallel pool.
//! `crates/gd_syntax/tests/test_layout.rs` fails if a file here is missing its `mod` line.

mod cache;
mod perf_scale;
mod project_model;
mod scene_index;
mod uid_only_ext_resource;
