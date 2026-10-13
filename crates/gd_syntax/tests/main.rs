//! The one integration-test binary for `gd_syntax`. Every `tests/*.rs` file is a module of it,
//! so the crate links a single test executable and all of its tests run in one parallel pool.
//! `crates/gd_syntax/tests/test_layout.rs` fails if a file here is missing its `mod` line.

mod annotation_arguments;
mod conformance;
mod dialect_delta;
mod fuzz_crate_isolation;
mod fuzz_smoke;
mod scope_enumeration;
mod test_layout;
