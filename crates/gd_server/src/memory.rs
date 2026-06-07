//! M5 WP-H1 memory budget — the soft/hard RSS caps that back the pressure ladder in
//! [`crate::observability::RssSampler::pressure`] and the bulk-eviction primitive in
//! [`crate::workspace::Workspace::evict_half`].
//!
//! Source-of-truth order, lowest → highest priority:
//!   1. Baked-in defaults ([`DEFAULT_SOFT_CAP_MB`], [`DEFAULT_HARD_CAP_MB`]) — fire only when both
//!      the bench file and the client overrides are absent.
//!   2. `bench/budget.toml::[memory]::soft_cap_mb` / `hard_cap_mb` (committed by WP-P5 from the
//!      WP-P1 capture on a large real-world project).
//!   3. `initializationOptions.memory.softCapMb` / `hardCapMb` (per-client overrides — the
//!      verification walk, a CI smoke run, etc.).
//!
//! TOML parsing is a tiny purpose-built section/key/value reader rather than a `toml` crate
//! dependency: the file format is fixed (we own it), the only keys consumed here are two integers,
//! and the workspace dep-audit policy (`docs` §3) is to add a new transitive only when avoiding
//! one would force significantly worse code. Mirrors the `project_godot` parser's minimal stance.

use std::path::Path;

use camino::Utf8Path;

use crate::config::MemoryConfig;

/// Fallback soft cap used when neither `bench/budget.toml` nor the client supplies a value. Set at
/// 2 GB so a fresh-clone session that hasn't yet measured against its target workspace still gets
/// a meaningful Soft-pressure trip before exhausting addressable memory on a 4 GB box.
pub const DEFAULT_SOFT_CAP_MB: u64 = 2_048;

/// Fallback hard cap — 4 GB. Double the soft cap, matching the `bench/budget.toml` ratio
/// (`soft = 2 × peak`, `hard = 4 × peak`) and giving a long-running session room to weather a
/// transient mass-reindex (`git checkout`, branch switch) before the LSP starts shedding.
pub const DEFAULT_HARD_CAP_MB: u64 = 4_096;

/// A byte count, as a `Copy` newtype. Exists so a byte value cannot be silently compared against
/// or assigned from a megabyte value: the WP-H1 ladder compares an [`crate::observability::RssSampler`]
/// reading against [`MemoryBudget::hard_cap_bytes`] — both [`Bytes`] — so a stray `hard_cap_mb()` at
/// the comparison site is a *type error*, not a silently-disabled Hard rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bytes(u64);

impl Bytes {
    /// Wrap a raw byte count. The field is private so a megabyte value can't be smuggled in as a
    /// byte count without going through [`Megabytes::to_bytes`] — the unit-safety guarantee the
    /// WP-H1 ladder relies on.
    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    /// The raw byte count, for formatting / arithmetic at the edges (logging, MB conversion).
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// A megabyte count — the unit the caps are configured and logged in. Convert to [`Bytes`] via
/// [`Megabytes::to_bytes`] before comparing against an RSS reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Megabytes(u64);

impl Megabytes {
    /// Wrap a raw megabyte count. Private field + [`Megabytes::to_bytes`] is the only MB→byte
    /// path, so the byte/MB unit distinction is enforced by the type system, not by convention.
    #[must_use]
    pub const fn new(mb: u64) -> Self {
        Self(mb)
    }

    /// MB → [`Bytes`]. `saturating_mul` so a malformed huge cap (e.g. a hand-edited
    /// `soft_cap_mb = 18000000000000`) degrades to a `u64::MAX` byte cap rather than wrapping to
    /// zero and tripping the ladder on the first sample.
    #[must_use]
    pub fn to_bytes(self) -> Bytes {
        Bytes::new(self.0.saturating_mul(1024 * 1024))
    }

    /// The raw MB count, for the startup info log + verification report.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Soft / Hard RSS caps. Constructed via [`MemoryBudget::resolve`], which layers
/// `initializationOptions.memory` over `bench/budget.toml::[memory]` over the baked-in defaults.
/// Caps are held as [`Megabytes`]; the byte-domain view ([`Self::soft_cap_bytes`]) is what the
/// ladder compares an RSS reading against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudget {
    soft_cap: Megabytes,
    hard_cap: Megabytes,
    source: BudgetSource,
}

/// Provenance flag attached to a [`MemoryBudget`] so the startup log can name the source the
/// operator can edit to change the ladder's trip points. Only consumed by
/// [`MemoryBudget::source`] — the ladder itself only cares about the resolved numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetSource {
    /// Both caps came from the baked-in defaults (no bench file, no client override).
    Default,
    /// At least one cap came from `bench/budget.toml`.
    BenchToml,
    /// At least one cap came from `initializationOptions.memory`.
    Client,
}

impl MemoryBudget {
    /// Resolve the effective budget from the three layered sources. `bench_path` is the absolute
    /// path the loader probes for the TOML; pass `None` to skip the file probe entirely (used by
    /// unit tests that don't want to write to disk).
    ///
    /// Numeric overflow on `* MB` is impossible in practice (`u64::MAX / (1024² )` is ~17 EB) but
    /// the conversion uses `saturating_mul` so a malformed `soft_cap_mb = 18000000000000` in the
    /// TOML degrades to a `u64::MAX` cap rather than wrapping to zero and tripping Soft on first
    /// sample.
    #[must_use]
    pub fn resolve(cfg: &MemoryConfig, bench_path: Option<&Utf8Path>) -> Self {
        let mut soft = DEFAULT_SOFT_CAP_MB;
        let mut hard = DEFAULT_HARD_CAP_MB;
        let mut source = BudgetSource::Default;
        if let Some(path) = bench_path {
            match load_bench_caps(path.as_std_path()) {
                Ok(Some((s, h))) => {
                    soft = s;
                    hard = h;
                    source = BudgetSource::BenchToml;
                }
                Ok(None) => {
                    // File is missing, or present with no `[memory]` caps at all — the documented
                    // WP-H1 fallback. One warn so a fresh-clone operator sees the ladder is using
                    // the safety defaults; do not pollute every session under a workspace that
                    // never bothered to ship a budget (the ladder still works either way). A
                    // *partial* `[memory]` section is a typo, not this case — it returns `Err` and
                    // lands in the louder arm below.
                    tracing::warn!(
                        name = "memory_budget_default",
                        bench_path = %path,
                        soft_cap_mb = soft,
                        hard_cap_mb = hard,
                        "bench/budget.toml absent or without [memory] caps; using built-in defaults for the memory pressure ladder",
                    );
                }
                Err(e) => {
                    // Present but malformed — louder than the missing-file path because a bench
                    // file that exists but doesn't parse is usually a hand-edit gone wrong, and
                    // silently falling back would hide an operator's typo from them.
                    tracing::warn!(
                        name = "memory_budget_invalid",
                        bench_path = %path,
                        error = %e,
                        soft_cap_mb = soft,
                        hard_cap_mb = hard,
                        "bench/budget.toml unreadable / malformed; using built-in defaults",
                    );
                }
            }
        }
        if let Some(s) = cfg.soft_cap_mb {
            soft = s;
            source = BudgetSource::Client;
        }
        if let Some(h) = cfg.hard_cap_mb {
            hard = h;
            source = BudgetSource::Client;
        }
        // Sanity gate: a misconfigured hard < soft would invert the ladder (Hard fires before
        // Soft and the operator never sees the eviction breadcrumb). Clamp so soft ≤ hard, with a
        // warn so the misconfiguration stays debuggable.
        if hard < soft {
            tracing::warn!(
                name = "memory_budget_inverted",
                soft_cap_mb = soft,
                hard_cap_mb = hard,
                "hard cap is below soft cap; clamping hard up to soft so the ladder is well-ordered",
            );
            hard = soft;
        }
        MemoryBudget {
            soft_cap: Megabytes::new(soft),
            hard_cap: Megabytes::new(hard),
            source,
        }
    }

    /// Soft cap in [`Bytes`] — what the ladder compares an [`crate::observability::RssSampler`] peak
    /// reading against. `saturating_mul` per the overflow note on [`Self::resolve`].
    #[must_use]
    pub fn soft_cap_bytes(&self) -> Bytes {
        self.soft_cap.to_bytes()
    }

    /// See [`Self::soft_cap_bytes`].
    #[must_use]
    pub fn hard_cap_bytes(&self) -> Bytes {
        self.hard_cap.to_bytes()
    }

    /// Raw cap in MB — exposed for the startup info log + verification report.
    #[must_use]
    pub fn soft_cap_mb(&self) -> u64 {
        self.soft_cap.get()
    }

    /// Raw cap in MB — exposed for the startup info log + verification report.
    #[must_use]
    pub fn hard_cap_mb(&self) -> u64 {
        self.hard_cap.get()
    }

    /// Provenance flag — see [`BudgetSource`].
    pub fn source(&self) -> BudgetSource {
        self.source
    }

    /// Convenience for tests + fixtures: build a budget directly from caps without the layered
    /// resolution. The provenance is set to [`BudgetSource::Client`] because that is the
    /// truthful match for "constructed in code" — a unit test override is, semantically, an
    /// explicit configuration choice.
    #[must_use]
    pub fn from_caps_mb(soft_cap_mb: u64, hard_cap_mb: u64) -> Self {
        let hard_cap_mb = hard_cap_mb.max(soft_cap_mb);
        MemoryBudget {
            soft_cap: Megabytes::new(soft_cap_mb),
            hard_cap: Megabytes::new(hard_cap_mb),
            source: BudgetSource::Client,
        }
    }
}

/// Read `bench/budget.toml`, extract `[memory]::soft_cap_mb` and `[memory]::hard_cap_mb`, and
/// return them as `(soft, hard)` MB. Three outcomes, each routing the caller to a distinct path:
///   * `Ok(Some((s, h)))` — both caps present; the bench file wins.
///   * `Ok(None)` — the file is genuinely absent (the fresh-clone case) *or* present with no
///     `[memory]` caps at all. Either way the operator hasn't configured memory, so the caller
///     quietly uses the built-in defaults.
///   * `Err(_)` — the file exists but is unreadable, malformed, *or* its `[memory]` section sets
///     exactly one of the two caps. A half-configured section is an operator typo (wrote one key,
///     forgot the other), so it routes to the louder malformed-warn path — which names the missing
///     key — rather than the misleading "not found" breadcrumb that would silently discard it.
///
/// The parser is intentionally tiny: walk lines, skip `#` comments + blanks, track the current
/// `[section]`, and inside `[memory]` look up two specific integer keys. Anything else in the
/// file is ignored (forward-compatibility with future bench keys). Floats with `.` aren't recognised by intent —
/// the two consumed keys are documented as MB integers in the WP-P5 reference doc; if a future
/// edit introduces a float there, parsing fails loudly and the operator updates the schema or
/// the parser deliberately.
fn load_bench_caps(path: &Path) -> std::io::Result<Option<(u64, u64)>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut current_section: Option<&str> = None;
    let mut soft: Option<u64> = None;
    let mut hard: Option<u64> = None;
    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let name = rest.strip_suffix(']').ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "bench/budget.toml:{}: unterminated section header",
                        line_no + 1
                    ),
                )
            })?;
            current_section = Some(section_owned(name));
            continue;
        }
        if current_section != Some("memory") {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim();
        let value = line[eq + 1..].trim();
        match key {
            "soft_cap_mb" => soft = Some(parse_u64(value, line_no)?),
            "hard_cap_mb" => hard = Some(parse_u64(value, line_no)?),
            _ => {}
        }
    }
    match (soft, hard) {
        (Some(s), Some(h)) => Ok(Some((s, h))),
        // Neither cap set: the file has no `[memory]` config at all (e.g. it only carries
        // `[cold_index]` bench data). Indistinguishable from "operator didn't configure memory",
        // so fall through to the quiet defaults path like a missing file.
        (None, None) => Ok(None),
        // Exactly one cap set: an operator typo (wrote one key, forgot the other). Surface it as
        // `Err` so the caller routes to the louder malformed-warn path — naming the missing key —
        // instead of the misleading "not found" breadcrumb that silently discards the half-config.
        (soft, _) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "[memory] section is incomplete: {present} is set but {missing} is missing — \
                 set both soft_cap_mb and hard_cap_mb, or neither",
                present = if soft.is_some() {
                    "soft_cap_mb"
                } else {
                    "hard_cap_mb"
                },
                missing = if soft.is_some() {
                    "hard_cap_mb"
                } else {
                    "soft_cap_mb"
                },
            ),
        )),
    }
}

/// Section names in `bench/budget.toml` are ASCII identifiers; this lifetime-stretching helper
/// just stores a `&'static str` for the canonical sections this loader cares about. Anything not
/// in the canonical list comes back as a sentinel that won't match the `"memory"` check above —
/// so an unrecognised section is silently skipped rather than allocating a new `String` per line.
fn section_owned(name: &str) -> &'static str {
    match name {
        "memory" => "memory",
        _ => "_other",
    }
}

/// Parse a u64 with a clear error message tied to the source line. The line number is the
/// 0-based loop index that the caller passed; we render it 1-based for the operator-facing log.
fn parse_u64(value: &str, line_no: usize) -> std::io::Result<u64> {
    value.parse::<u64>().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "bench/budget.toml:{}: expected an unsigned integer, got {value:?} ({e})",
                line_no + 1
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn defaults_apply_when_no_bench_file_and_no_client_override() {
        let cfg = MemoryConfig::default();
        let budget = MemoryBudget::resolve(&cfg, None);
        assert_eq!(budget.soft_cap_mb(), DEFAULT_SOFT_CAP_MB);
        assert_eq!(budget.hard_cap_mb(), DEFAULT_HARD_CAP_MB);
        assert_eq!(budget.source(), BudgetSource::Default);
    }

    #[test]
    fn client_override_wins_over_bench_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "[memory]").unwrap();
        writeln!(f, "soft_cap_mb = 100").unwrap();
        writeln!(f, "hard_cap_mb = 200").unwrap();
        drop(f);
        let cfg = MemoryConfig {
            cache_capacity: None,
            soft_cap_mb: Some(7),
            hard_cap_mb: Some(9),
        };
        let path = Utf8Path::from_path(&path).unwrap();
        let budget = MemoryBudget::resolve(&cfg, Some(path));
        assert_eq!(budget.soft_cap_mb(), 7);
        assert_eq!(budget.hard_cap_mb(), 9);
        assert_eq!(budget.source(), BudgetSource::Client);
    }

    #[test]
    fn bench_file_values_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "# leading comment").unwrap();
        writeln!(f, "[cold_index]").unwrap();
        writeln!(f, "observed_ms = 3340").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "[memory]").unwrap();
        writeln!(f, "# trailing comment").unwrap();
        writeln!(f, "baseline_rss_mb = 9.6").unwrap();
        writeln!(f, "peak_rss_mb = 291").unwrap();
        writeln!(f, "soft_cap_mb = 582").unwrap();
        writeln!(f, "hard_cap_mb = 1164").unwrap();
        drop(f);
        let cfg = MemoryConfig::default();
        let path = Utf8Path::from_path(&path).unwrap();
        let budget = MemoryBudget::resolve(&cfg, Some(path));
        assert_eq!(budget.soft_cap_mb(), 582);
        assert_eq!(budget.hard_cap_mb(), 1164);
        assert_eq!(budget.source(), BudgetSource::BenchToml);
        // Bytes conversion sanity: 582 MB == 610 271 232 bytes.
        assert_eq!(budget.soft_cap_bytes().get(), 582 * 1024 * 1024);
        assert_eq!(budget.hard_cap_bytes().get(), 1164 * 1024 * 1024);
    }

    #[test]
    fn missing_bench_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.toml");
        let cfg = MemoryConfig::default();
        let path = Utf8PathBuf::from_path_buf(path).unwrap();
        let budget = MemoryBudget::resolve(&cfg, Some(&path));
        assert_eq!(budget.soft_cap_mb(), DEFAULT_SOFT_CAP_MB);
        assert_eq!(budget.hard_cap_mb(), DEFAULT_HARD_CAP_MB);
        assert_eq!(budget.source(), BudgetSource::Default);
    }

    #[test]
    fn partial_memory_section_falls_back_to_defaults() {
        // `[memory]` is present but only soft is set. Both keys must appear for the bench file to
        // win; a half-config is treated as an operator typo, so it routes through the malformed
        // (`Err`) warn path and `resolve` falls back to defaults — the operator gets a breadcrumb
        // naming the missing key instead of a silently half-applied budget.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "[memory]").unwrap();
        writeln!(f, "soft_cap_mb = 100").unwrap();
        drop(f);
        let cfg = MemoryConfig::default();
        let path = Utf8Path::from_path(&path).unwrap();
        let budget = MemoryBudget::resolve(&cfg, Some(path));
        assert_eq!(budget.soft_cap_mb(), DEFAULT_SOFT_CAP_MB);
        assert_eq!(budget.hard_cap_mb(), DEFAULT_HARD_CAP_MB);
    }

    #[test]
    fn partial_memory_section_is_an_error_not_a_silent_default() {
        // Regression: a `[memory]` section with exactly one of the two caps
        // must surface from `load_bench_caps` as `Err(InvalidData)` naming the missing key — NOT
        // `Ok(None)`, which the caller renders as the misleading "not found" warning while
        // silently discarding the operator's value.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "[memory]").unwrap();
        writeln!(f, "soft_cap_mb = 300").unwrap();
        drop(f);
        let err =
            load_bench_caps(&path).expect_err("a half-configured [memory] section is an error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("hard_cap_mb"),
            "the error must name the missing key so the operator knows what to fix; got: {err}"
        );
    }

    #[test]
    fn bench_file_without_memory_section_is_ok_none() {
        // A budget.toml that exists but carries no `[memory]` caps at all is indistinguishable
        // from "operator didn't configure memory" — `load_bench_caps` returns `Ok(None)` (quiet
        // defaults), not an error. Only a *half*-configured section is the typo case.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "[cold_index]").unwrap();
        writeln!(f, "observed_ms = 3340").unwrap();
        drop(f);
        assert!(
            load_bench_caps(&path)
                .expect("readable file must not error")
                .is_none(),
            "a file with no [memory] caps must resolve to Ok(None), not Err"
        );
    }

    #[test]
    fn inverted_hard_cap_is_clamped_to_soft() {
        let cfg = MemoryConfig {
            cache_capacity: None,
            soft_cap_mb: Some(500),
            hard_cap_mb: Some(100),
        };
        let budget = MemoryBudget::resolve(&cfg, None);
        assert_eq!(budget.soft_cap_mb(), 500);
        assert_eq!(
            budget.hard_cap_mb(),
            500,
            "hard < soft must be clamped to soft so the ladder stays well-ordered"
        );
    }

    #[test]
    fn from_caps_mb_clamps_inverted() {
        let b = MemoryBudget::from_caps_mb(800, 200);
        assert_eq!(b.soft_cap_mb(), 800);
        assert_eq!(b.hard_cap_mb(), 800);
    }

    use camino::Utf8PathBuf;
}
