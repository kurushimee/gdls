# gd_server integration tests

Every file here is a module of the one `integration` binary (`tests/main.rs`), so all of them run in one process, 32 at a time, next to hundreds of other server sessions.

- Reach the shared helpers with `use crate::common;`, since `tests/main.rs` declares `common` once for the whole binary.
- Anything that needs a file URI goes through `common::file_uri` (over `gd_server::uri::path_to_file_uri`), never a hand-built `format!("file://…")`. A Windows temp root is a drive path and does not survive that.
- A session with no project of its own boots on `common::scratch_root()`. A server with no root falls back to the working directory, which under `cargo test` is this crate, so it would index and watch the fixtures and write a `.gdls/` cache into the tree.
- A `stubCacheDir` inside the project root goes under `.gdls/` (`root.join(".gdls").join("stub-cache")`), a folder the watcher ignores. Anywhere else in the root, each page the server writes shows up as a new project script once the watcher catches up, and the diagnostics it re-publishes can land where the test expects its next message.
