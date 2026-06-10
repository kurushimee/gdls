//! `initializationOptions` schema (`docs/05-lsp-cc-integration.md` §3). Parsing is deliberately
//! defensive: malformed options fall back to defaults with a logged warning and never fail the
//! `initialize` handshake.

use std::num::NonZeroUsize;

use serde::Deserialize;

/// Options passed by the client under `initializationOptions`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InitializationOptions {
    /// `res://` root. Optional; falls back to the workspace folder / nearest `project.godot` / cwd.
    pub project_root: Option<String>,
    /// Path to Godot's `extension_api.json`. Optional. When set it is honored verbatim (the
    /// user pinned a file — auto-dump never runs); when absent the v1.0.1 auto-dump resolution
    /// takes over (`crate::api_dump`), falling back to dynamic native types only when every
    /// source misses.
    pub extension_api_path: Option<String>,
    /// Path to a Godot 4.x executable for the `extension_api.json` auto-dump. Optional; when
    /// absent, discovery falls back to the `GDLS_GODOT` env var, then `godot4`/`godot` on PATH.
    pub godot_binary_path: Option<String>,
    /// v1.0.1: automatically dump `extension_api.json` (with project context, so GDExtension
    /// classes are captured) into `.gdls/` when no explicit `extensionApiPath` is set and the
    /// cached dump is missing or stale. Default **true** — the whole point is that the user
    /// never configures native types; set false to forbid gdls from ever spawning Godot.
    pub auto_dump_extension_api: bool,
    /// Diagnostics strictness (consumed starting in M3).
    pub strict: StrictConfig,
    /// M5 WP-O3 / WP-O4 — per-call analyzer knobs the LSP server threads into
    /// [`gd_analyze::analyze_with_options`]. Empty by default (the analyzer uses its baked-in
    /// safe values: [`gd_analyze::DEFAULT_ITER_LIMIT`] for the governor cap, no cancellation).
    pub analyzer: AnalyzerConfig,
    /// M5 WP-H1 / WP-H2 — memory hardening: LRU cache capacity + soft/hard RSS budget overrides.
    /// Both fields are optional; absent values fall through to the [`bench/budget.toml`]
    /// reference numbers (WP-P5) and finally to baked-in defaults
    /// ([`MemoryConfig::cache_capacity`], [`super::memory::DEFAULT_SOFT_CAP_MB`],
    /// [`super::memory::DEFAULT_HARD_CAP_MB`]).
    pub memory: MemoryConfig,
}

/// Manual so `parse(None)`, `parse(Some({}))`, and a missing single field all agree —
/// `auto_dump_extension_api` defaults TRUE, which a derived `Default` (false) would silently
/// disagree with under the container-level `#[serde(default)]`.
impl Default for InitializationOptions {
    fn default() -> Self {
        InitializationOptions {
            project_root: None,
            extension_api_path: None,
            godot_binary_path: None,
            auto_dump_extension_api: true,
            strict: StrictConfig::default(),
            analyzer: AnalyzerConfig::default(),
            memory: MemoryConfig::default(),
        }
    }
}

/// Per-call analyzer knobs surfaced through `initializationOptions.analyzer`. Optional; the
/// defaults preserve M3 behaviour.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AnalyzerConfig {
    /// M5 WP-O3: per-file fixpoint iteration budget. `None` → use the analyzer's default
    /// ([`gd_analyze::DEFAULT_ITER_LIMIT`] = 100 000). The LSP server passes this through to
    /// every per-file [`Workspace::analyze`](crate::workspace::Workspace::analyze) call so an
    /// operator can dial it down for a debug session (e.g. force-tripping the governor on a
    /// suspected runaway file) or up for an unusually-large bundled feature file.
    pub iter_limit: Option<u32>,
}

/// Memory-hardening knobs surfaced through `initializationOptions.memory`. All fields optional;
/// the [`Workspace`](crate::workspace::Workspace) builder layers each on top of
/// [`bench/budget.toml`](super::memory::MemoryBudget::resolve)'s reference numbers, and
/// the budget itself layers on top of a baked-in default so the ladder is always well-defined
/// even on a fresh clone with no bench file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryConfig {
    /// M5 WP-H2: bound on the per-Workspace `parse_cache` + `analysis_cache`. `None` → the WP-H2
    /// default of [`MemoryConfig::DEFAULT_CACHE_CAPACITY`] entries, which is well above any
    /// realistic open-buffer count (the v1 deployment target — a large real-world project, 2 338
    /// `.gd` files — keeps under ~50 open at peak) but bounded so a session that opens transient
    /// files for nav can't grow either cache without limit.
    pub cache_capacity: Option<usize>,
    /// M5 WP-H1: soft cap (MB). When unset, the loader reads
    /// `bench/budget.toml::[memory]::soft_cap_mb`; when neither exists, falls back to
    /// [`super::memory::DEFAULT_SOFT_CAP_MB`]. At Soft pressure the ladder evicts half of both
    /// caches via LRU order and emits one `tracing::warn!(name="memory_soft_cap_evicted")`.
    pub soft_cap_mb: Option<u64>,
    /// M5 WP-H1: hard cap (MB). Same fall-through chain as `soft_cap_mb` (
    /// [`super::memory::DEFAULT_HARD_CAP_MB`]). At Hard pressure request-driven full analyses are
    /// refused with LSP `ContentModified` (-32801); diagnostics publish parser output plus cached
    /// analysis when available, without starting new analysis. One
    /// `tracing::error!(name="memory_pressure_shed")` per transition into Hard so the pressure
    /// event is visible in the trace without spamming every shed.
    pub hard_cap_mb: Option<u64>,
}

impl MemoryConfig {
    /// WP-H2 default LRU capacity (per cache). Bounded but generous: the typical open-buffer
    /// count fits inside an order of magnitude; the bound exists to cap the long-running drift
    /// from transient nav reads, not to throttle the steady state.
    pub const DEFAULT_CACHE_CAPACITY: usize = 512;

    /// Resolved cache capacity as a [`NonZeroUsize`]: client override → default, clamping a
    /// client-supplied `0` (or an absent value) up to [`Self::DEFAULT_CACHE_CAPACITY`]. Returning
    /// `NonZeroUsize` makes the "never zero" invariant (`lru::LruCache::new` panics on a zero cap)
    /// unrepresentable at the call site — the caller can't fumble it into a runtime panic.
    #[must_use]
    pub fn cache_capacity(&self) -> NonZeroUsize {
        self.cache_capacity
            .and_then(NonZeroUsize::new)
            .unwrap_or_else(|| {
                NonZeroUsize::new(Self::DEFAULT_CACHE_CAPACITY)
                    .expect("invariant: DEFAULT_CACHE_CAPACITY is a nonzero constant")
            })
    }
}

/// Strict-mode configuration (`docs/04-diagnostics-strict-mode.md` §3).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StrictConfig {
    pub profile: StrictProfile,
    /// Fine-grained overrides, layered on top of the profile (resolved against warning names in M3).
    pub enable_warnings: Vec<String>,
    pub disable_warnings: Vec<String>,
    pub error_warnings: Vec<String>,
}

/// The diagnostics profile. Default is `godot` (pure parity with the project's own warning config).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StrictProfile {
    #[default]
    Godot,
    Strict,
    Off,
}

impl InitializationOptions {
    /// Parse from the raw `initializationOptions` JSON value. Never fails: invalid input logs a
    /// warning and yields defaults.
    pub fn parse(value: Option<&serde_json::Value>) -> Self {
        match value {
            Some(v) => serde_json::from_value(v.clone()).unwrap_or_else(|e| {
                log::warn!("invalid initializationOptions, using defaults: {e}");
                Self::default()
            }),
            None => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_none_yields_defaults() {
        let opts = InitializationOptions::parse(None);
        assert!(opts.project_root.is_none());
        assert_eq!(opts.strict.profile, StrictProfile::Godot);
        assert!(opts.memory.soft_cap_mb.is_none());
    }

    #[test]
    fn parse_valid_options_round_trips() {
        let v = serde_json::json!({
            "projectRoot": "res://",
            "strict": { "profile": "strict" },
            "memory": { "cacheCapacity": 256, "softCapMb": 100, "hardCapMb": 200 },
        });
        let opts = InitializationOptions::parse(Some(&v));
        assert_eq!(opts.project_root.as_deref(), Some("res://"));
        assert_eq!(opts.strict.profile, StrictProfile::Strict);
        assert_eq!(opts.memory.cache_capacity, Some(256));
        assert_eq!(opts.memory.soft_cap_mb, Some(100));
        assert_eq!(opts.memory.hard_cap_mb, Some(200));
    }

    /// The load-bearing invariant (CLAUDE.md "never crash, never lie", `docs/05` §3): a malformed
    /// payload must NEVER fail `initialize`. Each case below is type-wrong in a way that makes
    /// `serde_json::from_value::<InitializationOptions>` return `Err`; `parse` must swallow it
    /// (logging a warning) and fall back to `Self::default()` rather than propagating the error up
    /// into a failed handshake. Without this test, dropping a `#[serde(default)]` or the
    /// `unwrap_or_else` would ship a server that rejects `initialize` on bad options.
    #[test]
    fn malformed_options_fall_back_to_defaults_without_failing() {
        let cases = [
            serde_json::json!({ "strict": 42 }), // wrong type for a struct field
            serde_json::json!({ "memory": "not-an-object" }), // wrong type for `memory`
            serde_json::json!({ "memory": { "cacheCapacity": "ten" } }), // wrong type, nested
            serde_json::json!({ "strict": { "profile": "bogus" } }), // not a StrictProfile variant
            serde_json::json!({ "analyzer": { "iterLimit": -5 } }), // u32 can't be negative
            serde_json::json!([1, 2, 3]),        // top-level non-object
            serde_json::json!("a bare string"),  // top-level scalar
            serde_json::json!(true),
        ];
        for case in &cases {
            let opts = InitializationOptions::parse(Some(case));
            // Falls back to the FULL default — never panics, never propagates an error.
            assert!(
                opts.project_root.is_none(),
                "case {case:?} should default project_root"
            );
            assert_eq!(
                opts.strict.profile,
                StrictProfile::Godot,
                "case {case:?} should default strict"
            );
            assert!(
                opts.memory.soft_cap_mb.is_none() && opts.memory.cache_capacity.is_none(),
                "case {case:?} should default memory"
            );
        }
    }

    /// A `cacheCapacity` of 0 (or absent) clamps up to the default rather than panicking
    /// `lru::LruCache::new`. Pins the `NonZeroUsize` invariant at the type boundary.
    #[test]
    fn cache_capacity_clamps_zero_and_absent_to_default() {
        let zero = InitializationOptions::parse(Some(&serde_json::json!({
            "memory": { "cacheCapacity": 0 }
        })));
        assert_eq!(
            zero.memory.cache_capacity().get(),
            MemoryConfig::DEFAULT_CACHE_CAPACITY
        );
        let absent = InitializationOptions::parse(None);
        assert_eq!(
            absent.memory.cache_capacity().get(),
            MemoryConfig::DEFAULT_CACHE_CAPACITY
        );
    }
}
