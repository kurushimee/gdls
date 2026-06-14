//! `initializationOptions` schema (`docs/05-lsp-cc-integration.md` §3). Parsing is deliberately
//! defensive: malformed options fall back to defaults with a logged warning and never fail the
//! `initialize` handshake.

use std::num::NonZeroUsize;

use serde::Deserialize;

/// Options passed by the client under `initializationOptions`.
///
/// M7 (#59) — runtime re-config: `workspace/didChangeConfiguration` (or the
/// `workspace/configuration` pull) re-reads this same schema mid-session, but only the
/// **runtime-reloadable** subset applies: `strict.*`, `analyzer.*`, `memory.*`, `inlayHint.*`.
/// The remaining fields are **session-structural** (`projectRoot`, `extensionApiPath`,
/// `godotBinaryPath`, `autoDumpExtensionApi`, `embeddedApiFallback`, `stubCacheDir`): each is
/// baked into `Workspace::load` / watcher arming / background-dump topology at startup, so a
/// runtime payload that changes one keeps the old value and logs a "requires restart" warning.
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
    /// v1.0.2: when every native-API source misses (no `extensionApiPath`, no cached dump, no
    /// auto-dump, no project-root file), fall back to a bundled stock 4.6.3 `extension_api.json`
    /// instead of an empty DB, so builtins (`Node`, `Timer`, …) always resolve on a fresh
    /// install. Default **true**; set false to reproduce the bare-DB degraded mode (tests,
    /// memory-floor measurements).
    pub embedded_api_fallback: bool,
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
    /// v1.0.4 (#34): override the root under which native-class API stubs are materialized
    /// (default: the user-level gdls cache — `%LOCALAPPDATA%\gdls` / `~/.cache/gdls`). The
    /// in-process integration tests point this at a tempdir; end users normally leave it unset.
    pub stub_cache_dir: Option<String>,
    /// M8 (#64): `textDocument/completion` rendering knobs (snippet placeholders, the call-argument
    /// placeholder style). Anti-catalog W17 forbids coupling to the editor's own settings, so these
    /// are the documented `initializationOptions` defaults gdls picks instead. Read live on each
    /// completion request (a render-time concern), so unlike `strict`/`analyzer`/`memory` it is not
    /// part of the runtime-reload set — the startup value stands for the session.
    pub completion: CompletionConfig,
    /// M10 (#73): `textDocument/inlayHint` toggles (inferred-type hints, parameter-name hints).
    /// Unlike `completion` (read live per request), this IS part of the runtime-reload set:
    /// toggling either knob via `workspace/didChangeConfiguration` re-applies here and emits a
    /// `workspace/inlayHint/refresh` so the client re-requests with the new policy live.
    pub inlay_hint: InlayHintConfig,
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
            embedded_api_fallback: true,
            strict: StrictConfig::default(),
            analyzer: AnalyzerConfig::default(),
            memory: MemoryConfig::default(),
            stub_cache_dir: None,
            completion: CompletionConfig::default(),
            inlay_hint: InlayHintConfig::default(),
        }
    }
}

/// M10 (#73): the two independent inlay-hint toggles surfaced through `initializationOptions.inlayHint`.
/// Both default **true** (the rust-analyzer/gopls default — hints on, the user dials them down). A
/// runtime-reloadable group: a change to either toggle re-applies via
/// [`super::server::apply_runtime_config`] and triggers a `workspace/inlayHint/refresh` so the
/// already-displayed hints update without a restart.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InlayHintConfig {
    /// `InlayHintKind::TYPE` hints: the inferred type on a `var x := …` declaration and on an
    /// inferred `for` loop variable. Default **true**.
    pub type_hints: bool,
    /// `InlayHintKind::PARAMETER` hints: parameter-name labels before each argument at a resolved
    /// call site (`move(10, 20)` → `x:`/`y:`). Default **true**. Single-argument calls never get a
    /// parameter hint regardless of this toggle (a deliberate noise cut — see
    /// [`crate::inlay_hint`]).
    pub parameter_hints: bool,
}

impl Default for InlayHintConfig {
    fn default() -> Self {
        InlayHintConfig {
            type_hints: true,
            parameter_hints: true,
        }
    }
}

/// M8 (#64): completion rendering knobs surfaced through `initializationOptions.completion`. All
/// fields have defaults, so an absent `completion` section (the common case) yields the gopls-style
/// behaviour: call snippets with a `$0` final tab-stop, only emitted when the client advertises
/// `completionItem.snippetSupport`. The defaults are deliberate (anti-catalog W17: gdls owns the
/// canonical style rather than reading the editor's settings).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompletionConfig {
    /// Whether to emit snippet placeholders (`($0)`, `(${1:x}, ${2:y})`) for callable completions
    /// at all. Gated a SECOND time by the client's `completionItem.snippetSupport`: this knob lets
    /// a user who finds the auto-inserted parens noisy turn them off even on a snippet-capable
    /// client. Default **true** (the gopls/rust-analyzer default — accepting a function inserts its
    /// call parens and drops the cursor inside).
    pub snippets: bool,
    /// How a callable's call parentheses are rendered when `snippets` (and the client capability)
    /// are on. Default [`CallArgumentStyle::ParensWithCursor`] — gopls-style `($0)` so accepting
    /// `foo` yields `foo()` with the cursor between the parens.
    pub call_argument_style: CallArgumentStyle,
}

impl Default for CompletionConfig {
    fn default() -> Self {
        CompletionConfig {
            snippets: true,
            call_argument_style: CallArgumentStyle::default(),
        }
    }
}

/// The placeholder rendering for a callable completion's call parentheses (M8 #64). Only consulted
/// when snippets are enabled on both sides; the plain-text fallback (no snippet support) always
/// inserts a bare name with no parens, independent of this setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CallArgumentStyle {
    /// `name($0)` — insert the call parens with the final tab-stop between them (gopls default).
    #[default]
    ParensWithCursor,
    /// `name()` — insert empty call parens with no tab-stop (the cursor lands after the `)`).
    Parens,
    /// `name` — insert only the bare name; the user types the `(` themselves.
    NameOnly,
}

/// Per-call analyzer knobs surfaced through `initializationOptions.analyzer`. Optional; the
/// defaults preserve M3 behaviour.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AnalyzerConfig {
    /// M5 WP-O3: per-file fixpoint iteration budget. `None` → use the analyzer's default
    /// ([`gd_analyze::DEFAULT_ITER_LIMIT`] = 100 000). The LSP server passes this through to
    /// every per-file [`Workspace::analyze`](crate::workspace::Workspace::analyze) call so an
    /// operator can dial it down for a debug session (e.g. force-tripping the governor on a
    /// suspected runaway file) or up for an unusually-large bundled feature file.
    pub iter_limit: Option<u32>,
    /// M7 (#57) test/diagnostic governor: microseconds to sleep at every 256-node analyzer
    /// checkpoint, making each analyze pass deterministically slow. This is what lets the
    /// cancellation/staleness wire races be tested over a real session
    /// (`tests/concurrent_dispatch.rs`) and lets an operator simulate a pathologically slow
    /// project. `None` (the default — leave it unset in production) costs one branch per
    /// checkpoint. The value is deliberately unbounded (it exists to break responsiveness on
    /// purpose), but note the scale: a large file crosses hundreds of gates, so even a few
    /// thousand µs per gate makes every analysis-priced request feel hung in a live editor.
    pub checkpoint_delay_us: Option<u64>,
}

/// Memory-hardening knobs surfaced through `initializationOptions.memory`. All fields optional;
/// the [`Workspace`](crate::workspace::Workspace) builder layers each on top of
/// [`bench/budget.toml`](super::memory::MemoryBudget::resolve)'s reference numbers, and
/// the budget itself layers on top of a baked-in default so the ladder is always well-defined
/// even on a fresh clone with no bench file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
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

    /// M7 (#59): parse a RUNTIME re-configuration payload (`workspace/didChangeConfiguration` /
    /// the `workspace/configuration` pull). Unlike [`Self::parse`], malformed input is an `Err`
    /// — the runtime contract is **keep the previous configuration** (plus a logged warning and
    /// a `window/showMessage`), never a silent reset to defaults mid-session.
    pub fn parse_runtime(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value.clone())
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
        // M8 (#64): completion defaults — snippets on, gopls-style parens.
        assert!(opts.completion.snippets);
        assert_eq!(
            opts.completion.call_argument_style,
            CallArgumentStyle::ParensWithCursor
        );
        // M10 (#73): inlay-hint defaults — both toggles on.
        assert!(opts.inlay_hint.type_hints);
        assert!(opts.inlay_hint.parameter_hints);
    }

    #[test]
    fn parse_inlay_hint_toggles_round_trip() {
        let v = serde_json::json!({
            "inlayHint": { "typeHints": false, "parameterHints": true }
        });
        let opts = InitializationOptions::parse(Some(&v));
        assert!(!opts.inlay_hint.type_hints);
        assert!(opts.inlay_hint.parameter_hints);
    }

    /// A malformed `inlayHint` group falls back to the FULL default (both on), never failing
    /// `initialize` — the same "never crash, never lie" contract the other groups hold.
    #[test]
    fn malformed_inlay_hint_falls_back_to_defaults() {
        for case in [
            serde_json::json!({ "inlayHint": "not-an-object" }),
            serde_json::json!({ "inlayHint": { "typeHints": 7 } }),
        ] {
            let opts = InitializationOptions::parse(Some(&case));
            assert!(
                opts.inlay_hint.type_hints && opts.inlay_hint.parameter_hints,
                "case {case:?} should default inlayHint"
            );
        }
    }

    #[test]
    fn parse_completion_knobs_round_trip() {
        let v = serde_json::json!({
            "completion": { "snippets": false, "callArgumentStyle": "nameOnly" }
        });
        let opts = InitializationOptions::parse(Some(&v));
        assert!(!opts.completion.snippets);
        assert_eq!(
            opts.completion.call_argument_style,
            CallArgumentStyle::NameOnly
        );
    }

    /// A malformed `completion` group falls back to the FULL default (snippets on), never failing
    /// `initialize` — the same "never crash, never lie" contract the other groups hold.
    #[test]
    fn malformed_completion_falls_back_to_defaults() {
        for case in [
            serde_json::json!({ "completion": "not-an-object" }),
            serde_json::json!({ "completion": { "callArgumentStyle": "bogus" } }),
            serde_json::json!({ "completion": { "snippets": 7 } }),
        ] {
            let opts = InitializationOptions::parse(Some(&case));
            assert!(
                opts.completion.snippets,
                "case {case:?} should default completion"
            );
            assert_eq!(
                opts.completion.call_argument_style,
                CallArgumentStyle::ParensWithCursor
            );
        }
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
