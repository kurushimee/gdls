//! The session trail and the builtin-member audit.
//!
//! A builtin type can lose its members in a long-running session and stay that way until restart
//! (#622): `Vector3.ZERO` starts reporting `Cannot find member "ZERO" in base "Vector3".` in every
//! file. Nothing about the cause is known in advance, so this module does not test any one theory.
//! It does two cheap things instead:
//!
//! - [`Trail`] keeps the most recent state changes in memory (native DB swaps, project reloads,
//!   watcher reactions, cache invalidations, config changes, `class_name` changes, and every script
//!   reindex). Recording is a push onto a bounded ring; nothing is logged.
//! - [`audit_member_misses`] runs on every diagnostics computation and looks for the symptom
//!   itself: a member miss on a builtin base that the session's own data says should resolve.
//!   When it finds one, it logs a single `warn` with how the analyzer typed the base, what the
//!   native DB and the class registry say about the name, and the trail, so the event that
//!   preceded the break is in the log whatever it was.
//!
//! A genuine typo (`Vector3.ZER`) is absent from both the live DB and the stock surface, so it never
//! reaches the log.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::Instant;

use gd_analyze::{AnalysisResult, DataType};
use gd_syntax::ast::{NodeId, NodeKind, SubscriptAccess};
use gd_syntax::{ByteSpan, Dialect, ParseTree};
use gd_types::{NativeDb, NativeMember};
use lsp_types::Uri;
use rustc_hash::FxHashSet;

use crate::workspace::Workspace;

/// Entries kept per ring. Big enough to span a reload storm, small enough to print in one warning.
const STATE_CAP: usize = 64;
const FILE_CAP: usize = 48;
const NAME_CAP: usize = 48;

/// The recent-history ring buffers. See the module doc.
pub(crate) struct Trail {
    start: Instant,
    /// Rare, session-wide events: DB swaps, reloads, invalidations, config changes, reconciles.
    state: VecDeque<(f64, String)>,
    /// Frequent per-file events: reindexes and removals. Kept apart so a burst of edits cannot push
    /// the rare events out.
    files: VecDeque<(f64, String)>,
    /// `class_name` changes. Their own ring, so a branch switch that adds or drops hundreds of
    /// classes cannot push the rare state events out either.
    names: VecDeque<(f64, String)>,
    /// `(base, member, analysis generation, DB hash)` already reported, so one break logs once per
    /// state rather than once per keystroke.
    reported: FxHashSet<(String, String, u64, u64)>,
    /// The embedded stock surface for one dialect, ingested on first need (only when a builtin miss
    /// is absent from the live DB too).
    stock: Option<(Dialect, Option<NativeDb>)>,
}

impl Default for Trail {
    fn default() -> Self {
        Trail {
            start: Instant::now(),
            state: VecDeque::with_capacity(STATE_CAP),
            files: VecDeque::with_capacity(FILE_CAP),
            names: VecDeque::with_capacity(NAME_CAP),
            reported: FxHashSet::default(),
            stock: None,
        }
    }
}

impl Trail {
    /// Record a session-wide state change.
    pub(crate) fn state(&mut self, what: String) {
        let at = self.start.elapsed().as_secs_f64();
        push(&mut self.state, STATE_CAP, (at, what));
    }

    /// Record a script's `class_name` entering, leaving, or changing.
    pub(crate) fn name(&mut self, what: String) {
        let at = self.start.elapsed().as_secs_f64();
        push(&mut self.names, NAME_CAP, (at, what));
    }

    /// Record a per-file index change.
    pub(crate) fn file(&mut self, what: String) {
        let at = self.start.elapsed().as_secs_f64();
        push(&mut self.files, FILE_CAP, (at, what));
    }

    /// Every ring merged in time order, one event per line.
    fn render(&self) -> String {
        let mut all: Vec<(&f64, &str, &str)> = self
            .state
            .iter()
            .map(|(t, w)| (t, "state", w.as_str()))
            .chain(self.names.iter().map(|(t, w)| (t, "name ", w.as_str())))
            .chain(self.files.iter().map(|(t, w)| (t, "file ", w.as_str())))
            .collect();
        all.sort_by(|a, b| a.0.total_cmp(b.0));
        let mut out = String::new();
        for (t, ring, what) in all {
            let _ = writeln!(out, "    t+{t:.3}s {ring} {what}");
        }
        out
    }

    /// Whether the stock surface for `dialect` carries `base.member` in the shape `kind` needs.
    fn stock_has(&mut self, dialect: Dialect, base: &str, member: &str, kind: MissKind) -> bool {
        if self.stock.as_ref().is_none_or(|(d, _)| *d != dialect) {
            self.stock = Some((dialect, crate::api_dump::embedded_stock_db(dialect)));
        }
        self.stock
            .as_ref()
            .and_then(|(_, db)| db.as_ref())
            .is_some_and(|db| has_member(db, base, member, kind))
    }

    /// Append `other`'s events after this trail's own, for a workspace rebuild that would
    /// otherwise start the history over.
    pub(crate) fn append_from(&mut self, other: Trail) {
        let shift = other.start.duration_since(self.start).as_secs_f64();
        for (t, what) in other.state {
            push(&mut self.state, STATE_CAP, (t + shift, what));
        }
        for (t, what) in other.names {
            push(&mut self.names, NAME_CAP, (t + shift, what));
        }
        for (t, what) in other.files {
            push(&mut self.files, FILE_CAP, (t + shift, what));
        }
    }
}

fn push(ring: &mut VecDeque<(f64, String)>, cap: usize, entry: (f64, String)) {
    if ring.len() == cap {
        ring.pop_front();
    }
    ring.push_back(entry);
}

/// One line naming a native DB: release, provenance, size, and the content hash that identifies it.
pub(crate) fn describe_native(db: &NativeDb) -> String {
    let version = &db.header().version_full_name;
    format!(
        "{} ({:?}, {} classes of which {} from doc XML, {} builtins, dump hash {:016x})",
        if version.is_empty() {
            "unknown version"
        } else {
            version
        },
        db.provenance(),
        db.class_count(),
        db.doc_class_count(),
        db.builtin_count(),
        db.content_hash(),
    )
}

/// The three analyzer message shapes that report a member as absent from its base.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MissKind {
    /// `Cannot find member "X" in base "B".`
    Member,
    /// `Function "X()" not found in base B.`
    Function,
    /// `Static function "X()" not found in base "B".`
    StaticFunction,
}

/// `(member, base, kind)` from a member-miss message, or `None` for any other message.
fn parse_member_miss(msg: &str) -> Option<(&str, &str, MissKind)> {
    if let Some(rest) = msg.strip_prefix("Cannot find member \"") {
        let (member, rest) = rest.split_once("\" in base \"")?;
        let base = rest.strip_suffix("\".")?;
        return Some((member, base, MissKind::Member));
    }
    if let Some(rest) = msg.strip_prefix("Static function \"") {
        let (member, rest) = rest.split_once("()\" not found in base \"")?;
        let base = rest.strip_suffix("\".")?;
        return Some((member, base, MissKind::StaticFunction));
    }
    if let Some(rest) = msg.strip_prefix("Function \"") {
        let (member, rest) = rest.split_once("()\" not found in base ")?;
        let base = rest.strip_suffix('.')?;
        return Some((member, base, MissKind::Function));
    }
    None
}

/// Look for a member miss on a builtin base that the session's own data contradicts, and log
/// everything that could explain it. See the module doc.
pub(crate) fn audit_member_misses(
    ws: &mut Workspace,
    uri: &Uri,
    text: &str,
    tree: &ParseTree,
    result: &AnalysisResult,
) {
    for d in &result.diagnostics {
        let Some((member, base, kind)) = parse_member_miss(d.message()) else {
            continue;
        };
        // `Array[int]` and friends are the builtin before the bracket.
        let base = base.split('[').next().unwrap_or(base);
        if gd_analyze::resolver::builtin_type_from_name(base).is_none() {
            continue;
        }
        let dialect = ws.dialect;
        let Some(verdict) = verdict(&ws.native, &mut ws.trail, dialect, base, member, kind) else {
            continue; // a genuine miss: absent everywhere gdls knows of
        };
        let key = (
            base.to_owned(),
            member.to_owned(),
            ws.analysis_generation(),
            ws.native.content_hash(),
        );
        if !ws.trail.reported.insert(key) {
            continue;
        }
        log::warn!(
            "builtin member audit (#622): {} reports {:?} — {verdict}\n{}",
            uri.as_str(),
            d.message(),
            context(ws, text, tree, result, d.span(), base),
        );
    }
}

/// Whether `db` resolves `base.member` the way the message's access needs it: a static call
/// needs a static method, anything else any member of that name.
fn has_member(db: &NativeDb, base: &str, member: &str, kind: MissKind) -> bool {
    match (kind, db.lookup_builtin_member(base, member).map(|(_, m)| m)) {
        (MissKind::StaticFunction, Some(NativeMember::Method(m))) => m.is_static,
        (MissKind::StaticFunction, _) => false,
        (_, found) => found.is_some(),
    }
}

/// Why a miss of `base.member` contradicts the session's data, or `None` for a genuine miss.
fn verdict(
    native: &NativeDb,
    trail: &mut Trail,
    dialect: Dialect,
    base: &str,
    member: &str,
    kind: MissKind,
) -> Option<&'static str> {
    if has_member(native, base, member, kind) {
        Some("the live native DB HAS this member, so the analyzer contradicts it")
    } else if trail.stock_has(dialect, base, member, kind) {
        Some("the live native DB LACKS this member, but the stock surface for this release has it")
    } else {
        None
    }
}

/// The evidence block for one contradicted miss.
fn context(
    ws: &Workspace,
    text: &str,
    tree: &ParseTree,
    result: &AnalysisResult,
    span: ByteSpan,
    base: &str,
) -> String {
    let mut out = String::new();
    let start = span.start.min(text.len());
    let line_no = text[..start].bytes().filter(|&b| b == b'\n').count() + 1;
    let line = text[..start]
        .rfind('\n')
        .map_or(0, |i| i + 1)
        .min(text.len());
    let line_text = text[line..].lines().next().unwrap_or("").trim();
    let _ = writeln!(out, "  at line {line_no}: {line_text}");

    // How the analyzer typed the base expression of the access the diagnostic sits on.
    match base_node_of(tree, span) {
        Some(base_id) => {
            let node = match &tree.get(base_id).kind {
                NodeKind::Identifier(i) => format!("identifier `{}`", i.name),
                other => format!("{:?}", std::mem::discriminant(other)),
            };
            let _ = writeln!(
                out,
                "  analyzer typed the base ({node}) as: {}",
                describe_type(ws, result.types.get(base_id))
            );
        }
        None => {
            let _ = writeln!(out, "  base node: not found from the diagnostic's span");
        }
    }

    // What the session's name sources say about the base name.
    let native = &ws.native;
    let _ = writeln!(out, "  live native DB: {}", describe_native(native));
    match native.builtin_named(base) {
        Some(bt) => {
            let _ = writeln!(
                out,
                "  builtin `{base}` in the live DB: {} members, {} methods, {} constants, {} enums",
                bt.members.len(),
                bt.methods.len(),
                bt.constants.len(),
                bt.enums.len()
            );
        }
        None => {
            let _ = writeln!(out, "  builtin `{base}` in the live DB: ABSENT");
        }
    }
    match native.class_named(base) {
        Some(nc) => {
            let _ = writeln!(
                out,
                "  native CLASS `{base}` in the live DB: PRESENT (api_type {:?}, {} methods, {} \
                 constants) — it shadows the builtin",
                nc.api_type,
                nc.methods.len(),
                nc.constants.len()
            );
        }
        None => {
            let _ = writeln!(out, "  native class `{base}` in the live DB: none");
        }
    }
    match ws.index.registry().get(base) {
        Some(entry) => {
            let _ = writeln!(
                out,
                "  class_name registry: `{base}` -> {} (resolves as {:?})",
                entry.path,
                ws.index.resolve_name(base, native)
            );
        }
        None => {
            let _ = writeln!(out, "  class_name registry: no `{base}`");
        }
    }
    let (parse_len, analysis_len) = ws.cache_lens();
    let _ = writeln!(
        out,
        "  dialect {}, analysis generation {}, cached parses {parse_len}, cached analyses \
         {analysis_len}, indexed scripts {}",
        ws.dialect,
        ws.analysis_generation(),
        ws.index.file_count(),
    );
    let _ = writeln!(out, "  recent events, oldest first:");
    out.push_str(&ws.trail.render());
    out
}

/// The base of the attribute access whose attribute identifier spans `span`.
fn base_node_of(tree: &ParseTree, span: ByteSpan) -> Option<NodeId> {
    tree.iter_ids().find_map(|id| match &tree.get(id).kind {
        NodeKind::Subscript(sub) => match sub.access {
            Some(SubscriptAccess::Attribute(Some(attr))) if tree.get(attr).span == span => sub.base,
            _ => None,
        },
        _ => None,
    })
}

fn describe_type(ws: &Workspace, dt: &DataType) -> String {
    let script = dt.script_type.as_ref().map(|s| {
        let path = ws
            .index
            .path(s.file)
            .map_or_else(|| format!("{:?}", s.file), |p| p.to_string());
        if s.inner.is_empty() {
            path
        } else {
            format!("{path}::{}", s.inner.join("."))
        }
    });
    format!(
        "kind {:?}, builtin {:?}, native {:?}, script {:?}, meta {}, source {:?}",
        dt.kind, dt.builtin_type, dt.native_type, script, dt.is_meta_type, dt.type_source
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_three_member_miss_shapes() {
        assert_eq!(
            parse_member_miss(r#"Cannot find member "ZERO" in base "Vector3"."#),
            Some(("ZERO", "Vector3", MissKind::Member))
        );
        assert_eq!(
            parse_member_miss(r#"Static function "from_angle()" not found in base "Vector2"."#),
            Some(("from_angle", "Vector2", MissKind::StaticFunction))
        );
        assert_eq!(
            parse_member_miss(r#"Function "length()" not found in base Vector3."#),
            Some(("length", "Vector3", MissKind::Function))
        );
        assert_eq!(parse_member_miss("Identifier \"x\" not declared."), None);
    }

    fn trimmed_db() -> NativeDb {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../gd_types/tests/fixtures/trimmed_api.json");
        NativeDb::load(path.to_str().unwrap()).unwrap()
    }

    #[test]
    fn a_miss_the_live_db_contradicts_is_flagged() {
        let db = trimmed_db();
        let mut t = Trail::default();
        let d = Dialect::DEFAULT;
        assert!(verdict(&db, &mut t, d, "Vector3", "ZERO", MissKind::Member)
            .is_some_and(|v| v.contains("HAS")));
    }

    #[test]
    fn a_miss_only_the_stock_surface_has_is_flagged() {
        // An empty live DB stands in for one that lost the member.
        let mut t = Trail::default();
        let d = Dialect::DEFAULT;
        assert!(verdict(
            &NativeDb::empty(),
            &mut t,
            d,
            "Vector3",
            "ZERO",
            MissKind::Member
        )
        .is_some_and(|v| v.contains("LACKS")));
    }

    #[test]
    fn a_genuine_typo_is_not_flagged() {
        let db = trimmed_db();
        let mut t = Trail::default();
        let d = Dialect::DEFAULT;
        assert_eq!(
            verdict(&db, &mut t, d, "Vector3", "ZER", MissKind::Member),
            None
        );
        // An instance method called as a static is Godot's own error, not a contradiction.
        assert_eq!(
            verdict(
                &db,
                &mut t,
                d,
                "Vector3",
                "length",
                MissKind::StaticFunction
            ),
            None
        );
    }

    #[test]
    fn the_trail_is_bounded_and_keeps_the_newest() {
        let mut t = Trail::default();
        for i in 0..(STATE_CAP + 5) {
            t.state(format!("event {i}"));
        }
        assert_eq!(t.state.len(), STATE_CAP);
        assert_eq!(t.state.front().map(|e| e.1.as_str()), Some("event 5"));
    }
}
