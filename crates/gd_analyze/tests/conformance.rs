//! Analyze-phase conformance harness — the M3 sibling of `gd_syntax`'s parse-phase harness.
//!
//! Diffs [`gd_analyze::analyze`] output against Godot's vendored golden-file corpus
//! (`tests/conformance/corpus/analyzer/`, see its `PROVENANCE.md`). The oracle and comparison
//! semantics are ported from `modules/gdscript/tests/gdscript_test_runner.cpp`:
//!
//! * skip `*.notest.gd` (multi-file companions, no `.out`);
//! * pair the `.out` by swapping the final extension;
//! * **classify by the `.out` first line, never by directory**:
//!   - `GDTEST_OK` ⇒ zero errors + exactly the `~~ WARNING` set the `.out` lists (runtime stdout after
//!     the warning lines is stripped);
//!   - `GDTEST_ANALYZER_ERROR` ⇒ exactly the `>> ERROR` (+ any `~~ WARNING`) lines, in order;
//!   - anything else (`GDTEST_PARSER_ERROR`, compiler/runtime/load) ⇒ skipped (the parser harness owns
//!     parser errors).
//!
//! gdls diagnostics render to Godot's exact line format and compare ordered. The native DB is the
//! committed `trimmed_api.json` fixture; cross-file resolution uses [`NoCrossFile`] for now — WP-C
//! resolves every base via the native DB or in-file scope, so single-file analysis is faithful, and
//! the cross-file companion cases sit in `analyze_known_failures.txt` until cross-file depth lands
//! (WP-D/E).
//!
//! Ratchet (hybrid, identical mechanism to the parser harness): a per-file `analyze_known_failures.txt`
//! regression net (primary) plus an aggregate `analyze_fidelity_floor.txt` (secondary).
//! `GDLS_BLESS_CONFORMANCE=1` prints the regenerated state to stdout for a human to commit.

use gd_syntax::{Dialect, ParseOptions};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use camino::{Utf8Path, Utf8PathBuf};
use gd_analyze::{
    CrossFileQuery, MemberName, MemberXref, Severity, StrictSettings, SyntacticQuery, WarnPolicy,
};
use gd_project::{FileId, Index, Interface, WarningConfig};
use gd_types::NativeDb;

/// What the `.out` pins about the analyze phase. `None` from [`classify`] ⇒ not an analyze-phase case.
enum Expect {
    /// `GDTEST_OK` — zero errors and exactly these `~~ WARNING` lines.
    Ok(Vec<String>),
    /// `GDTEST_ANALYZER_ERROR` — exactly these `>>`/`~~` diagnostic lines, in order.
    AnalyzerError(Vec<String>),
}

fn conformance_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance")
}

/// The committed native-DB fixture (1203 classes), loaded once and shared across the parallel tests.
fn native_db() -> &'static NativeDb {
    static DB: OnceLock<NativeDb> = OnceLock::new();
    DB.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../gd_types/tests/fixtures/trimmed_api.json");
        NativeDb::load(path.to_str().expect("utf-8 path")).unwrap_or_else(|e| {
            panic!(
                "load native DB fixture at {}: {e}\n(needed by the analyze conformance harness)",
                path.display()
            )
        })
    })
}

/// Godot's test-runner warning policy
/// (`tests/gdscript_test_runner.cpp:150-159` in Godot): demote every warning to `Warn`
/// (including the four error-by-default ones) **except** `UNTYPED_DECLARATION` /
/// `INFERRED_DECLARATION`, which keep their `Ignore` defaults. This is what makes the corpus's
/// `~~ WARNING …` lines reproducible for warnings that default to Error in production (the four
/// `inference_on_variant` / `native_method_override` / `get_node_default_without_onready` /
/// `onready_with_export` codes); without this demotion the corpus would expect `>> ERROR` lines.
fn policy(dialect: Dialect) -> WarnPolicy {
    use gd_project::WarnLevel as ProjLevel;
    let mut config = WarningConfig::default();
    for &name in gd_analyze::warnings::WARN_NAMES.iter() {
        // UNTYPED_DECLARATION and INFERRED_DECLARATION stay at their `Ignore` defaults — Godot
        // skips them at line 153 of the test runner.
        if name == "UNTYPED_DECLARATION" || name == "INFERRED_DECLARATION" {
            continue;
        }
        config
            .levels
            .insert(name.to_ascii_lowercase(), ProjLevel::Warn);
    }
    WarnPolicy::build(&config, &StrictSettings::default(), dialect)
}

fn collect_gd_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_gd_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("gd") {
            out.push(path);
        }
    }
}

/// Keep only the diagnostic lines (`>> ERROR …` / `~~ WARNING …`), in order, dropping the status
/// header and any trailing runtime stdout — the Godot runner's own curation.
fn diag_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    lines
        .map(str::trim_end)
        .filter(|l| l.starts_with(">>") || l.starts_with("~~"))
        .map(str::to_string)
        .collect()
}

fn classify(out_content: &str) -> Option<Expect> {
    let mut lines = out_content.lines();
    match lines.next().unwrap_or("").trim() {
        "GDTEST_OK" => Some(Expect::Ok(diag_lines(lines))),
        "GDTEST_ANALYZER_ERROR" => Some(Expect::AnalyzerError(diag_lines(lines))),
        _ => None,
    }
}

/// Render one gdls diagnostic into Godot's `.out` line format. `line_override` (WP-R3) takes
/// precedence over the byte-derived line — used for emissions that mirror Godot's null-source
/// `push_error` path (gdscript_parser.cpp:241-244) where the line comes from the parser's
/// `previous` token at end-of-parse, not from the diagnostic's source-range bytes. Otherwise we
/// derive the 1-based line by counting newlines up to `start` (tab expansion affects only columns,
/// which the `.out` omits).
fn render(
    severity: Severity,
    code: &str,
    message: &str,
    source: &str,
    start: usize,
    line_override: Option<u32>,
) -> String {
    let line = line_override.map(|l| l as usize).unwrap_or_else(|| {
        1 + source.as_bytes()[..start.min(source.len())]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
    });
    match severity {
        Severity::Error => format!(">> ERROR at line {line}: {message}"),
        Severity::Warning => format!("~~ WARNING at line {line}: ({code}) {message}"),
    }
}

/// Corpus-aware cross-file query — wraps a [`SyntacticQuery`] over the cold-indexed corpus and
/// overrides path resolution to handle relative paths (the corpus fixtures use sibling-relative
/// strings like `"inner_base.gd"` or `"../features/foo.gd"`, never `res://`). Everything else
/// delegates to the default impls (`resolve_inner_chain`, `lookup_file_member`, `lookup_file_enum`,
/// `is_file_tool`, `is_file_abstract` all derive from `interface()`).
///
/// One `CorpusQuery` is constructed per fixture being analyzed — `fixture_dir` is the absolute
/// directory of that fixture, used as the base for resolving relative `extends "…"` / `preload(…)`
/// path literals via `Utf8Path::join` + canonicalization.
struct CorpusQuery<'a> {
    inner: SyntacticQuery<'a>,
    fixture_dir: &'a Utf8Path,
}

impl<'a> CorpusQuery<'a> {
    fn new(index: &'a Index, native: &'a NativeDb, fixture_dir: &'a Utf8Path) -> Self {
        CorpusQuery {
            inner: SyntacticQuery::new(index, native),
            fixture_dir,
        }
    }
}

/// Clean an absolute path of `.` / `..` components without filesystem canonicalization. We avoid
/// `std::fs::canonicalize` because on Windows it returns a UNC-prefixed path (`\\?\C:\…`) that
/// doesn't match the Index's normalized keys (`C:/…` with forward slashes after `normalize`).
fn clean_path_components(path: &Utf8Path) -> Utf8PathBuf {
    use camino::Utf8Component;
    let mut out = Utf8PathBuf::new();
    for comp in path.components() {
        match comp {
            Utf8Component::ParentDir => {
                out.pop();
            }
            Utf8Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

impl CrossFileQuery for CorpusQuery<'_> {
    fn global_class_file(&self, name: &str) -> Option<FileId> {
        self.inner.global_class_file(name)
    }
    fn interface(&self, file: FileId) -> Option<&Interface> {
        self.inner.interface(file)
    }
    fn file_path(&self, file: FileId) -> Option<&str> {
        self.inner.file_path(file)
    }
    fn resolve_res_path(&self, path: &str) -> Option<FileId> {
        // `res://` literal: defer to the Index's existing res:// resolver.
        if path.starts_with("res://") {
            return self.inner.resolve_res_path(path);
        }
        // Sibling-relative: join against the fixture's directory, clean (handles `..`/`./`),
        // then look up by absolute path in the Index. The Index's own normalize step then
        // replaces backslashes with forward slashes for the hashtable lookup. Mirrors Godot's
        // `ResourceLoader::load(p_path, p_type, …)` relative-to-current-resource semantics.
        let joined = self.fixture_dir.join(path);
        let cleaned = clean_path_components(&joined);
        self.inner.index.file_id(&cleaned)
    }
    fn member_initializer_xrefs(&self, file: FileId, member: &str) -> Vec<MemberXref> {
        // WP-R2: parse `file` on demand, find its `member`'s initializer, and collect
        // `(target_file, target_name)` pairs for every `CONST.NAME` attribute access whose
        // `CONST` is bound to a `preload(...)` constant declared in the same class.
        //
        // Mirrors Godot's recursive `resolve_class_member` -> remote-analyzer hop
        // (gdscript_analyzer.cpp:1001-1024) — gdls's analyzer doesn't reach into another
        // file's analyzer, so we walk the depended file's AST structurally instead. The
        // conformance harness is the only consumer; the gd_server impl can carry a deeper
        // cache when it lands.
        let Some(path) = self.inner.index.path(file) else {
            return Vec::new();
        };
        let path_buf = path.to_path_buf();
        let source = match fs::read_to_string(path_buf.as_std_path()) {
            Ok(s) => s,
            // A genuinely-missing depended file (deleted between index build and analyze) is
            // the expected silent-empty path. Other I/O errors (locked, perms, non-UTF-8) would
            // silently drop the cycle diagnostic — log them so an operator can see why a
            // diagnostic Godot would emit isn't surfacing.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                eprintln!(
                    "conformance: xref scan skipped for {path_buf}: {e}; cycle diagnostic on `{member}` may not fire"
                );
                return Vec::new();
            }
        };
        collect_member_initializer_xrefs(&path_buf, &source, member, self.inner.index)
    }
}

/// Parse `source` (the contents of `path`) and walk the root class for `member`'s initializer,
/// returning cross-file references that `member` reads via a `preload`-constant chain. Used by
/// [`CorpusQuery::member_initializer_xrefs`] to power WP-R2's cross-file cycle detection.
fn collect_member_initializer_xrefs(
    path: &Utf8Path,
    source: &str,
    member: &str,
    index: &Index,
) -> Vec<MemberXref> {
    use gd_syntax::ast::{Member, NodeId, NodeKind, SubscriptAccess};
    use gd_syntax::token::Literal;
    use gd_syntax::ParseTree;

    let tree = gd_syntax::parse(source).tree;
    let Some(root_id) = tree.root_id() else {
        return Vec::new();
    };
    let root = tree.get(root_id);
    let NodeKind::Class(class) = &root.kind else {
        return Vec::new();
    };

    let file_dir = path.parent().map(Utf8Path::to_path_buf).unwrap_or_default();
    let resolve_path_lit = |lit: &str| -> Option<FileId> {
        if lit.starts_with("res://") {
            return index.resolve_res_path(lit);
        }
        let joined = file_dir.join(lit);
        let cleaned = clean_path_components(&joined);
        index.file_id(&cleaned)
    };

    // Build name -> FileId for `const X = preload("...")` constants in this class, while
    // also locating the named member's initializer node.
    let mut preloads: std::collections::HashMap<String, FileId> = std::collections::HashMap::new();
    let mut target_init: Option<NodeId> = None;
    let ident_name = |tree: &ParseTree, id: Option<NodeId>| -> Option<String> {
        let id = id?;
        if let NodeKind::Identifier(i) = &tree.get(id).kind {
            Some(i.name.clone())
        } else {
            None
        }
    };

    for m in &class.members {
        match m {
            Member::Constant(cid) => {
                let n = tree.get(*cid);
                if let NodeKind::Constant(c) = &n.kind {
                    let name = ident_name(&tree, c.identifier);
                    if let (Some(name), Some(init)) = (name.clone(), c.initializer) {
                        if let NodeKind::Preload(p) = &tree.get(init).kind {
                            if let Some(path_id) = p.path {
                                if let NodeKind::Literal(lit) = &tree.get(path_id).kind {
                                    if let Literal::String(s) = &lit.value {
                                        if let Some(fid) = resolve_path_lit(s) {
                                            preloads.insert(name.clone(), fid);
                                        }
                                    }
                                }
                            }
                        }
                        if name == member {
                            target_init = Some(init);
                        }
                    }
                }
            }
            Member::Variable(vid) => {
                let n = tree.get(*vid);
                if let NodeKind::Variable(v) = &n.kind {
                    let name = ident_name(&tree, v.identifier);
                    if let (Some(name), Some(init)) = (name, v.initializer) {
                        if name == member {
                            target_init = Some(init);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let Some(init) = target_init else {
        return Vec::new();
    };

    // Walk `init` for `IDENT.ATTR` patterns where `IDENT` is a preload constant.
    let mut xrefs: Vec<MemberXref> = Vec::new();
    fn walk(
        tree: &ParseTree,
        node_id: NodeId,
        preloads: &std::collections::HashMap<String, FileId>,
        out: &mut Vec<MemberXref>,
    ) {
        let n = tree.get(node_id);
        match &n.kind {
            NodeKind::Subscript(s) => {
                if let (Some(base), Some(SubscriptAccess::Attribute(Some(attr_id)))) =
                    (s.base, s.access)
                {
                    if let NodeKind::Identifier(b) = &tree.get(base).kind {
                        if let Some(fid) = preloads.get(&b.name) {
                            if let NodeKind::Identifier(a) = &tree.get(attr_id).kind {
                                out.push(MemberXref {
                                    target_file: *fid,
                                    target_member: MemberName::from(a.name.clone()),
                                });
                            }
                        }
                    }
                    walk(tree, base, preloads, out);
                }
            }
            NodeKind::BinaryOp(b) => {
                if let Some(l) = b.left_operand {
                    walk(tree, l, preloads, out);
                }
                if let Some(r) = b.right_operand {
                    walk(tree, r, preloads, out);
                }
            }
            NodeKind::UnaryOp(u) => {
                if let Some(o) = u.operand {
                    walk(tree, o, preloads, out);
                }
            }
            NodeKind::Call(c) => {
                if let Some(callee) = c.callee {
                    walk(tree, callee, preloads, out);
                }
                for &arg in &c.arguments {
                    walk(tree, arg, preloads, out);
                }
            }
            NodeKind::TernaryOp(t) => {
                if let Some(x) = t.condition {
                    walk(tree, x, preloads, out);
                }
                if let Some(x) = t.true_expr {
                    walk(tree, x, preloads, out);
                }
                if let Some(x) = t.false_expr {
                    walk(tree, x, preloads, out);
                }
            }
            NodeKind::Cast(c) => {
                if let Some(o) = c.operand {
                    walk(tree, o, preloads, out);
                }
            }
            NodeKind::Array(a) => {
                for &e in &a.elements {
                    walk(tree, e, preloads, out);
                }
            }
            NodeKind::Dictionary(d) => {
                for kv in &d.elements {
                    if let Some(k) = kv.key {
                        walk(tree, k, preloads, out);
                    }
                    if let Some(v) = kv.value {
                        walk(tree, v, preloads, out);
                    }
                }
            }
            _ => {}
        }
    }
    walk(&tree, init, &preloads, &mut xrefs);
    xrefs
}

/// The cold-built corpus index, populated once. Mirrors the project-startup index `gd_server`
/// builds on `initialize`. Re-used across every fixture in the conformance run, so cross-file
/// fixtures (features/external_*, lookup_class, preload_enum_error, …) can resolve their peers.
fn corpus_index(suite: &Suite) -> &'static Index {
    static INDEXES: OnceLock<Vec<Index>> = OnceLock::new();
    let built = INDEXES.get_or_init(|| {
        SUITES
            .iter()
            .map(|s| {
                let root_buf = conformance_dir().join(s.dir);
                let root = Utf8Path::from_path(&root_buf)
                    .expect("corpus root utf-8")
                    .to_path_buf();
                let mut index = Index::build(&root);
                add_support_sources(&mut index, s.dialect);
                index
            })
            .collect()
    });
    &built[suite.slot]
}

/// Index `corpus/support/`, the sources Godot's own runner resolves from *outside* the vendored
/// subtree. `corpus/analyzer/` is a byte-for-byte mirror checked with a bare `diff -rq`, so a
/// helper the fixtures reference but upstream keeps a level higher cannot live inside it. The
/// fidelity pass only walks the suite directories, so nothing here is ever run as a case; the
/// index just needs the `class_name` and the signatures. #312.
fn add_support_sources(index: &mut Index, dialect: Dialect) {
    let dir = conformance_dir().join("corpus/support");
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .collect::<Result<Vec<_>, _>>()
        .expect("read corpus/support entries");
    let mut paths: Vec<PathBuf> = entries
        .into_iter()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "gd"))
        .collect();
    // Deterministic order: `read_dir` is filesystem-ordered, and two files declaring the same
    // `class_name` would otherwise register whichever landed last.
    paths.sort();
    assert!(
        !paths.is_empty(),
        "corpus/support has no .gd files — {} is what makes `Utils` resolvable",
        dir.display()
    );
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let utf8 = Utf8PathBuf::from_path_buf(path.clone())
            .unwrap_or_else(|_| panic!("support path not utf-8: {}", path.display()));
        let options = ParseOptions {
            dialect,
            ..Default::default()
        };
        index.set_interface_from_tree(&utf8, &gd_syntax::parse_with_options(&text, &options).tree);
    }
    // Recompute dependency edges now the registry carries the support class names too.
    index.finish_cold_index();
}

/// One corpus tree plus the dialect its goldens were generated at.
///
/// The oldest supported tag keeps the *full* vendored corpus; every newer tag carries only the
/// files that actually diverge, so a version bump is "vendor the new full corpus, demote the
/// previous one to its divergence subset" rather than a wholesale copy. Fidelity is one aggregate
/// number over every suite, so a file cannot be lost by moving between them.
struct Suite {
    /// Path under `tests/conformance/`.
    dir: &'static str,
    /// Prefix on every reported path, so a known-failures entry names its suite.
    tag: &'static str,
    dialect: Dialect,
    /// Index into the lazily built per-suite index vector.
    slot: usize,
    /// Take only the `GDTEST_ANALYZER_ERROR` cases from this tree, skipping its `GDTEST_OK` ones.
    ///
    /// Set only for a tree another harness owns. `corpus/parser/` is the parse phase's corpus, and
    /// its `GDTEST_OK` goldens pin the parse phase being silent; running them here would make this
    /// harness the owner of a second corpus it was never sized against, and its native-DB fixture
    /// does not carry the classes several of them name. The `GDTEST_ANALYZER_ERROR` goldens are
    /// different: no harness checks them at all today, because the parse phase cannot produce the
    /// error they pin. See #495 for the `GDTEST_OK` half.
    analyzer_errors_only: bool,
}

const SUITES: &[Suite] = &[
    Suite {
        dir: "corpus/analyzer",
        tag: "4.7",
        dialect: Dialect::Godot4_7,
        slot: 0,
        analyzer_errors_only: false,
    },
    Suite {
        dir: "corpus/analyzer-4.6",
        tag: "4.6",
        dialect: Dialect::Godot4_6,
        slot: 1,
        analyzer_errors_only: false,
    },
    // Godot classifies a case by its `.out` first line, never by the directory it sits in, and
    // three `GDTEST_ANALYZER_ERROR` goldens live under `parser/errors/` upstream — all three about
    // `@export*` applies, which run after the parse. The corpus is vendored once, under the crate
    // that owns the parse phase, so this suite reaches across to it rather than duplicating the
    // files: `corpus/analyzer/` is a byte-for-byte mirror of Godot's tree and a copy would break
    // the `diff -rq` that proves it. The parser harness reads the same files and skips them, for
    // the same reason and by the same rule.
    Suite {
        dir: "../../../gd_syntax/tests/conformance/corpus/parser",
        tag: "4.7-parser",
        dialect: Dialect::Godot4_7,
        slot: 2,
        analyzer_errors_only: true,
    },
];

/// Analyze one source and render its diagnostics to `.out` lines, in publish (offset) order.
/// `script_path` is the corpus file's basename (e.g. `enum_class_var_assign_with_wrong_enum_type.gd`),
/// passed through to the head class's `fqcn` so error messages render `<file.gd>.<EnumName>` to
/// match Godot's golden `.out` lines (WP-J — analyzer.cpp:702/147).
fn gdls_diag_lines(source: &str, script_path: &str, gd_path: &Path, suite: &Suite) -> Vec<String> {
    let tree = gd_syntax::parse_with_options(
        source,
        &gd_syntax::ParseOptions {
            dialect: suite.dialect,
            script_path: "",
        },
    )
    .tree;
    let index = corpus_index(suite);
    let fixture_dir_buf = Utf8PathBuf::from_path_buf(
        gd_path
            .parent()
            .expect("fixture has parent dir")
            .to_path_buf(),
    )
    .expect("fixture dir utf-8");
    let fixture_dir = clean_path_components(&fixture_dir_buf);
    let xfile = CorpusQuery::new(index, native_db(), &fixture_dir);

    // Look up the fixture's own FileId in the index (`ctx.file` semantics). The cold-built corpus
    // includes every `.gd` under the corpus root, so the fixture is always indexed; WP-RD2 threads
    // the `Option<FileId>` straight through (a miss — which never happens for the corpus — analyzes
    // as an orphan, `None`, instead of inventing a colliding placeholder id).
    let file_id = Utf8PathBuf::from_path_buf(gd_path.to_path_buf())
        .ok()
        .map(|p| clean_path_components(&p))
        .and_then(|p| index.file_id(&p));

    let result = gd_analyze::analyze_with_options(
        &tree,
        file_id,
        script_path,
        native_db(),
        &xfile,
        &policy(suite.dialect),
        gd_analyze::AnalyzeOptions {
            dialect: suite.dialect,
            ..Default::default()
        },
    );
    result
        .diagnostics
        .iter()
        .map(|d| {
            render(
                d.severity(),
                d.code(),
                d.message(),
                source,
                d.span().start,
                d.line(),
            )
        })
        .collect()
}

fn rel_path(corpus: &Path, gd: &Path) -> String {
    gd.strip_prefix(corpus)
        .unwrap_or(gd)
        .to_string_lossy()
        .replace('\\', "/")
}

fn read_lines_set(path: &Path) -> BTreeSet<String> {
    fs::read_to_string(path)
        .map(|c| {
            c.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn read_floor(path: &Path) -> f64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|c| {
            c.lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .and_then(|l| l.parse::<f64>().ok())
        })
        .unwrap_or(0.0)
}

/// The `N` in `>> ERROR at line N: …`, for the 4.7 runner's stable line sort.
fn out_line_number(line: &str) -> u32 {
    line.split_once("at line ")
        .and_then(|(_, rest)| rest.split_once(':'))
        .and_then(|(n, _)| n.trim().parse().ok())
        .unwrap_or(u32::MAX)
}

fn bullet_list(items: &BTreeSet<&String>) -> String {
    items
        .iter()
        .map(|s| format!("  {s}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn analyze_phase_fidelity() {
    let conformance = conformance_dir();

    let mut eligible = 0usize;
    let mut matched = 0usize;
    let mut skipped = 0usize;
    let mut total_files = 0usize;
    let mut failures: BTreeSet<String> = BTreeSet::new();
    let mut samples: Vec<String> = Vec::new();
    // For `GDLS_BLESS_FULL_DIFFS=1` dump (see bless block below).
    let mut all_diffs: Vec<String> = Vec::new();

    for suite in SUITES {
        let corpus = conformance.join(suite.dir);
        assert!(
            corpus.is_dir(),
            "corpus missing at {} — see PROVENANCE.md to vendor it",
            corpus.display()
        );

        let mut gd_files = Vec::new();
        collect_gd_files(&corpus, &mut gd_files);
        gd_files.sort();
        assert!(
            !gd_files.is_empty(),
            "no .gd files under {}",
            corpus.display()
        );
        total_files += gd_files.len();

        for gd in &gd_files {
            let name = gd.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if name.ends_with(".notest.gd") {
                skipped += 1;
                continue;
            }

            let rel = format!("{}/{}", suite.tag, rel_path(&corpus, gd));
            let source = fs::read_to_string(gd).expect("read .gd source");
            let out_path = gd.with_extension("out");
            let Ok(out_content) = fs::read_to_string(&out_path) else {
                skipped += 1; // no `.out` (e.g. a stray companion) — nothing to compare
                continue;
            };

            let Some(expect) = classify(&out_content) else {
                skipped += 1;
                continue;
            };

            if suite.analyzer_errors_only && !matches!(expect, Expect::AnalyzerError(_)) {
                skipped += 1;
                continue;
            }

            eligible += 1;
            // Godot's `parser->script_path` is the source-file path; the head class's `fqcn` derives
            // from it via `canonicalize_path` (analyzer.cpp:702), and the resulting `<file.gd>.<EnumName>`
            // rendering appears in the corpus's golden `.out` lines. We pass the basename so the
            // `Display for DataType` `get_file()` mirror produces the same string.
            let script_path = gd.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let mut got = gdls_diag_lines(&source, script_path, gd, suite);
            // gdscript_test_runner.cpp:571-585 — when the analyzer returns an error, Godot's
            // runner outputs only `>> ERROR …` lines and skips the `~~ WARNING …` block (the
            // `#ifdef DEBUG_ENABLED` warning loop sits AFTER the early return). The corpus's
            // `GDTEST_ANALYZER_ERROR` golden files reflect this — they only list errors. Drop
            // warnings from gdls's output for these cases so the harness comparison mirrors
            // Godot's stripping rather than treating any incidental warning as a regression.
            if matches!(expect, Expect::AnalyzerError(_)) {
                got.retain(|line| line.starts_with(">>"));
                // DIALECT(4.7): gdscript_test_runner.cpp:578-591 — the runner stable-sorts the
                // error list by `start_line` before printing, so within one line the primary
                // error comes first and cascading ones follow. 4.6 printed raw emission order.
                // This lives in Godot's *test runner*, not its analyzer, so it belongs here and
                // not in `DiagnosticSink`: real LSP output is unaffected.
                if suite.dialect >= Dialect::Godot4_7 {
                    got.sort_by_key(|line| out_line_number(line));
                }
            }
            let want = match &expect {
                Expect::Ok(warnings) => warnings,
                Expect::AnalyzerError(diags) => diags,
            };

            if &got == want {
                matched += 1;
            } else {
                if samples.len() < 40 {
                    samples.push(format!(
                        "  {rel}\n      want: {want:?}\n      got:  {got:?}"
                    ));
                }
                // For bless-mode full-diff dump.
                let mut diff_buf = String::new();
                diff_buf.push_str(&format!("===== {rel} =====\n"));
                diff_buf.push_str("--- want ---\n");
                for l in want {
                    diff_buf.push_str(l);
                    diff_buf.push('\n');
                }
                diff_buf.push_str("--- got ---\n");
                for l in &got {
                    diff_buf.push_str(l);
                    diff_buf.push('\n');
                }
                all_diffs.push(diff_buf);
                failures.insert(rel);
            }
        }
    }

    let fidelity = if eligible == 0 {
        1.0
    } else {
        matched as f64 / eligible as f64
    };
    let summary = format!(
        "analyze-phase fidelity: {matched}/{eligible} = {fidelity:.4}  \
         ({skipped} skipped, {total_files} total .gd)"
    );
    println!("{summary}");

    // Bless mode: emit the regenerated ratchet state for a human to commit. Never writes files.
    if std::env::var_os("GDLS_BLESS_CONFORMANCE").is_some() {
        let floor = (fidelity * 100.0).floor() / 100.0;
        println!(
            "\n----- BEGIN analyze_known_failures.txt ({} entries) -----",
            failures.len()
        );
        for f in &failures {
            println!("{f}");
        }
        println!("----- END analyze_known_failures.txt -----");
        println!(
            "----- BEGIN analyze_fidelity_floor.txt -----\n{floor:.2}\n----- END analyze_fidelity_floor.txt -----"
        );
        if std::env::var_os("GDLS_BLESS_FULL_DIFFS").is_some() {
            println!("\n----- BEGIN full want/got diffs -----");
            for d in &all_diffs {
                println!("{d}");
            }
            println!("----- END full want/got diffs -----");
        }
        return;
    }

    let known = read_lines_set(&conformance.join("analyze_known_failures.txt"));
    let floor = read_floor(&conformance.join("analyze_fidelity_floor.txt"));

    let new_regressions: BTreeSet<&String> = failures.difference(&known).collect();
    let newly_passing: BTreeSet<&String> = known.difference(&failures).collect();

    let mut problems: Vec<String> = Vec::new();
    if !new_regressions.is_empty() {
        problems.push(format!(
            "{} NEW analyze regression(s) — failing but not in analyze_known_failures.txt:\n{}",
            new_regressions.len(),
            bullet_list(&new_regressions)
        ));
    }
    if !newly_passing.is_empty() {
        problems.push(format!(
            "{} file(s) now PASS but are still listed in analyze_known_failures.txt (delete these lines):\n{}",
            newly_passing.len(),
            bullet_list(&newly_passing)
        ));
    }
    if fidelity + 1e-9 < floor {
        problems.push(format!(
            "fidelity {fidelity:.4} fell below floor {floor:.4}"
        ));
    }

    assert!(
        problems.is_empty(),
        "{summary}\n\n{}\n\nTo re-baseline: \
         GDLS_BLESS_CONFORMANCE=1 cargo test -p gd_analyze --test conformance -- --nocapture\n\n\
         sample mismatches:\n{}",
        problems.join("\n\n"),
        samples.join("\n"),
    );
}
