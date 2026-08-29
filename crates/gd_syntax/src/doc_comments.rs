//! M7 (#62) — `##` doc-comment association, ported from `GDScriptParser`'s `TOOLS_ENABLED`
//! doc pipeline at 4.6.3-stable. Runs **post-parse** over `(source, tree, lexer comments)`:
//! Godot attaches docs inline while parsing (`parse_class_member`,
//! `gdscript_parser.cpp:1048-1080`), but every rule is line-arithmetic over the tokenizer's
//! comment map and the produced AST extents, so a read-only pass that visits members in
//! declaration order — the parse order — reproduces it exactly without touching the ported
//! grammar (the token stream, and so both conformance ratchets, are unaffected).
//!
//! Ported rules (file:line references into `modules/gdscript/`):
//! - doc comment = `##` prefix, `has_comment(p_line, true)` (`gdscript_parser.cpp:4022`);
//! - member docs: inline on the declaration's start line, else the contiguous `new_line` `##`
//!   block ending right above it — hoisted above any annotations — gated by
//!   `min_member_doc_line` so two members never share a block (`:1048-1080`);
//! - inner-class docs: same shape via `parse_class_doc_comment`, no min check (`:1062-1068`);
//! - head class docs: the first `##` run in `0..=min(max_script_doc_line, first_member - 1)`
//!   (`:832-847`), where every member/value block parse clamps `max_script_doc_line` to just
//!   above its own block (`:4045`,`:4100`) so a member's doc is never re-read as the class doc;
//! - enum value docs: inline (only when the NEXT value sits on a later line), else the block
//!   above gated by `min_enum_value_doc_line` (`:1627-1650`);
//! - line processing: shared space-prefix strip, space joins (no join after `[br]`, newline
//!   join around `[codeblock]` fences), opaque `[code]`/`[codeblock]`/`[kbd]` content via the
//!   `DocLineState` machine (`_process_doc_line`, `:3905-4020`);
//! - `@deprecated[: msg]` / `@experimental[: msg]` in both kinds; `@tutorial[(Title)]: url`
//!   in class docs (`:4067-4088`, `:4122-4175`).
//!
//! Out of scope (deliberate): local var/const docs (`:2186` — interfaces carry no locals; their
//! `max_script_doc_line` clamps can never bind below the first member's line, so skipping them
//! is exact), and unnamed-enum value docs (Godot stores them class-level by value name; no
//! interface consumer exists yet).
//!
//! The stored prose keeps its **BBCode** markup — identical in kind to `extension_api.json`
//! descriptions, so one downstream converter (gd_server's `docs` module) serves both.

use std::collections::HashMap;

use crate::ast::{Member, NodeId, NodeKind, ParseTree};
use crate::dialect::Dialect;
use crate::lexer::CommentData;

/// `MemberDocData` (`gdscript_parser.h`): one member's doc prose + deprecation/experimental
/// markers. `description` is the Godot-processed, still-BBCode string. Serde + Hash so the
/// extracted copies can ride `gd_project`'s `Interface` (warm-start cache serialization).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct MemberDoc {
    pub description: String,
    pub is_deprecated: bool,
    pub deprecated_message: String,
    pub is_experimental: bool,
    pub experimental_message: String,
}

/// `ClassDocData`: brief/long prose split at the first blank `##` line, plus `@tutorial` links.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ClassDoc {
    pub brief: String,
    pub description: String,
    /// `(title, url)` pairs; the title may be empty (`@tutorial: url`).
    pub tutorials: Vec<(String, String)>,
    pub is_deprecated: bool,
    pub deprecated_message: String,
    pub is_experimental: bool,
    pub experimental_message: String,
}

/// Every doc association for one parse — carried on [`ParseTree::docs`] so the existing
/// interface-extraction call sites get docs with zero signature churn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocTable {
    /// Head class + inner classes, keyed by the class node.
    pub class_docs: HashMap<NodeId, ClassDoc>,
    /// var/const/func/signal/enum declarations, keyed by the declaration node.
    pub member_docs: HashMap<NodeId, MemberDoc>,
    /// Named-enum values, keyed by `(enum node, value index)`.
    pub enum_value_docs: HashMap<(NodeId, usize), MemberDoc>,
}

impl DocTable {
    pub fn is_empty(&self) -> bool {
        self.class_docs.is_empty() && self.member_docs.is_empty() && self.enum_value_docs.is_empty()
    }
}

/// `_process_doc_line`'s `DocLineState` (`gdscript_parser.cpp:3898`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum DocLineState {
    Normal,
    InCode,
    InCodeblock,
    InKbd,
}

struct Associator<'a> {
    source: &'a str,
    /// The dialect whose doc-comment rules apply — 4.7 changed how lines are trimmed and made
    /// `[br][br]` a paragraph break.
    dialect: Dialect,
    tree: &'a ParseTree,
    comments: &'a HashMap<u32, CommentData>,
    /// `GDScriptParser::min_member_doc_line` — bumped to `member.end_line + 1` after every
    /// member so consecutive members never claim the same block.
    min_member_doc_line: u32,
    /// `GDScriptParser::max_script_doc_line` — clamped to just above every parsed block so the
    /// head-class scan never re-reads a member's doc.
    max_script_doc_line: u32,
    out: DocTable,
}

/// Associate the lexer's recorded comments with the tree's declarations. Pure and read-only;
/// returns an empty table for an empty tree.
pub fn associate(source: &str, tree: &ParseTree, comments: &HashMap<u32, CommentData>) -> DocTable {
    associate_with_dialect(source, tree, comments, Dialect::DEFAULT)
}

/// [`associate`] under an explicit dialect.
pub fn associate_with_dialect(
    source: &str,
    tree: &ParseTree,
    comments: &HashMap<u32, CommentData>,
    dialect: Dialect,
) -> DocTable {
    let Some(root) = tree.root_id() else {
        return DocTable::default();
    };
    if comments.is_empty() {
        return DocTable::default();
    }
    let mut assoc = Associator {
        source,
        dialect,
        tree,
        comments,
        min_member_doc_line: 1,
        max_script_doc_line: u32::MAX,
        out: DocTable::default(),
    };
    assoc.walk_class(root);
    assoc.attach_head_class_doc(root);
    assoc.out
}

impl Associator<'_> {
    fn comment_text(&self, data: &CommentData) -> &str {
        self.source
            .get(data.span.start..data.span.end)
            .unwrap_or_default()
    }

    /// `has_comment(p_line, /*p_must_be_doc=*/true)` (`gdscript_parser.cpp:4022`).
    fn has_doc(&self, line: u32) -> bool {
        self.comments
            .get(&line)
            .is_some_and(|c| self.comment_text(c).starts_with("##"))
    }

    fn is_new_line_doc(&self, line: u32) -> bool {
        self.comments
            .get(&line)
            .is_some_and(|c| c.new_line && self.comment_text(c).starts_with("##"))
    }

    /// Walk one class's members in declaration order — the parse order Godot attaches docs in.
    fn walk_class(&mut self, class_id: NodeId) {
        let NodeKind::Class(class) = &self.tree.get(class_id).kind else {
            return;
        };
        for member in &class.members {
            match member {
                Member::Class(id) => {
                    // Godot parses the entire inner class (its members update
                    // `min_member_doc_line`) BEFORE `parse_class_member` attaches the inner
                    // class's own doc and bumps the min past its end.
                    self.walk_class(*id);
                    let (start, end, doc_line) = self.member_lines(*id);
                    if self.has_doc(start) {
                        let (doc, _) = self.parse_class_doc(start, true);
                        self.out.class_docs.insert(*id, doc);
                    } else if self.has_doc(doc_line) && self.is_new_line_doc(doc_line) {
                        // No `min_member_doc_line` check for classes (`:1066`).
                        let (doc, _) = self.parse_class_doc(doc_line, false);
                        self.out.class_docs.insert(*id, doc);
                    }
                    self.min_member_doc_line = end + 1;
                }
                Member::Constant(id)
                | Member::Function(id)
                | Member::Signal(id)
                | Member::Variable(id)
                | Member::Enum(id) => {
                    // Enum value docs first: Godot attaches them inside `parse_enum`, before
                    // `parse_class_member` attaches the enum declaration's own doc.
                    if matches!(self.tree.get(*id).kind, NodeKind::Enum(_)) {
                        self.attach_enum_value_docs(*id);
                    }
                    let (start, end, doc_line) = self.member_lines(*id);
                    if self.has_doc(start) {
                        let (doc, _) = self.parse_member_doc(start, true);
                        self.out.member_docs.insert(*id, doc);
                    } else if doc_line >= self.min_member_doc_line
                        && self.has_doc(doc_line)
                        && self.is_new_line_doc(doc_line)
                    {
                        let (doc, _) = self.parse_member_doc(doc_line, false);
                        self.out.member_docs.insert(*id, doc);
                    }
                    self.min_member_doc_line = end + 1;
                }
                // Unnamed-enum values and @export_group annotations take no doc and (matching
                // Godot, where neither flows through `parse_class_member`) do not bump the min.
                Member::EnumValue(_) | Member::Group(_) => {}
            }
        }
    }

    /// `(start_line, end_line, doc_comment_line)` for a member — `doc_comment_line` is
    /// `start_line - 1` hoisted above any annotation that sits at or above it (`:1049-1058`).
    fn member_lines(&self, id: NodeId) -> (u32, u32, u32) {
        let node = self.tree.get(id);
        let start = node.loc.start.line;
        let end = node.loc.end.line;
        let mut doc_line = start.saturating_sub(1);
        for ann in &node.annotations {
            let ann_start = self.tree.get(*ann).loc.start.line;
            if ann_start <= doc_line {
                doc_line = ann_start.saturating_sub(1);
            }
        }
        (start, end, doc_line)
    }

    /// Enum value docs (`gdscript_parser.cpp:1627-1650`), named enums only.
    fn attach_enum_value_docs(&mut self, enum_id: NodeId) {
        let NodeKind::Enum(enum_node) = &self.tree.get(enum_id).kind else {
            return;
        };
        if enum_node.identifier.is_none() {
            return; // Unnamed enum — out of scope (module doc).
        }
        // Godot seeds the gate from the token before the first value (the `{`, on the
        // declaration's first line in any parseable enum), i.e. "no block above the braces".
        let mut min_value_doc_line = self.tree.get(enum_id).loc.start.line + 1;
        let value_lines: Vec<Option<u32>> = enum_node
            .values
            .iter()
            .map(|v| v.identifier.map(|id| self.tree.get(id).loc.start.line))
            .collect();
        for (i, line) in value_lines.iter().enumerate() {
            let Some(value_line) = *line else { continue };
            let doc_line = value_line.saturating_sub(1);
            let mut doc = None;
            if self.has_doc(value_line) {
                // Inline doc comment — but only when it isn't followed by another value on the
                // SAME line (the comment belongs to the last value of that line, `:1635`).
                let next_on_same_line = value_lines
                    .get(i + 1)
                    .and_then(|l| *l)
                    .is_some_and(|next| next <= value_line);
                if !next_on_same_line {
                    doc = Some(self.parse_member_doc(value_line, true).0);
                }
            } else if doc_line >= min_value_doc_line
                && self.has_doc(doc_line)
                && self.is_new_line_doc(doc_line)
            {
                doc = Some(self.parse_member_doc(doc_line, false).0);
            }
            if let Some(doc) = doc {
                self.out.enum_value_docs.insert((enum_id, i), doc);
            }
            min_value_doc_line = value_line + 1;
        }
    }

    /// The head class doc scan (`gdscript_parser.cpp:830-847`) — runs after every member has
    /// been processed, so `max_script_doc_line` carries all the block clamps.
    fn attach_head_class_doc(&mut self, root: NodeId) {
        let node = self.tree.get(root);
        let NodeKind::Class(class) = &node.kind else {
            return;
        };
        let mut max_line = node.loc.end.line;
        if let Some(first) = class.members.first() {
            if let Some(first_line) = self.member_line(first) {
                max_line = self.max_script_doc_line.min(first_line.saturating_sub(1));
            }
        }
        let mut line = 1u32;
        while line <= max_line {
            if self.is_new_line_doc(line) {
                // Extend the run downward to its last contiguous `##` line.
                while line < max_line && self.is_new_line_doc(line + 1) {
                    line += 1;
                }
                let (doc, _) = self.parse_class_doc(line, false);
                self.out.class_docs.insert(root, doc);
                break;
            }
            line += 1;
        }
    }

    /// `ClassNode::Member::get_line()` for the first-member bound of the head scan.
    fn member_line(&self, member: &Member) -> Option<u32> {
        let id = match member {
            Member::Class(id)
            | Member::Constant(id)
            | Member::Function(id)
            | Member::Signal(id)
            | Member::Variable(id)
            | Member::Enum(id)
            | Member::Group(id) => *id,
            Member::EnumValue(v) => v.identifier?,
        };
        Some(self.tree.get(id).loc.start.line)
    }

    /// Walk a block upward to its first line and compute the shared space prefix
    /// (`gdscript_parser.cpp:4036-4056`). Returns `(block_start, space_prefix)`.
    fn block_start(&mut self, p_line: u32, single_line: bool) -> (u32, String) {
        let mut line = p_line;
        if !single_line {
            while line > 1 && self.is_new_line_doc(line - 1) {
                line -= 1;
            }
        }
        self.max_script_doc_line = self.max_script_doc_line.min(line.saturating_sub(1));
        let first = self
            .comments
            .get(&line)
            .map(|c| self.comment_text(c))
            .unwrap_or_default();
        let after_hashes = first.strip_prefix("##").unwrap_or(first);
        let spaces = after_hashes.len() - after_hashes.trim_start_matches(' ').len();
        (line, " ".repeat(spaces))
    }

    /// `parse_doc_comment` (`gdscript_parser.cpp:4033`). Returns the doc and the block's first
    /// line (already clamped into `max_script_doc_line`).
    fn parse_member_doc(&mut self, p_line: u32, single_line: bool) -> (MemberDoc, u32) {
        let (start, space_prefix) = self.block_start(p_line, single_line);
        let mut state = DocLineState::Normal;
        let mut result = MemberDoc::default();
        for line in start..=p_line {
            let Some(data) = self.comments.get(&line) else {
                continue;
            };
            let raw = self.comment_text(data);
            let doc_line = raw.strip_prefix("##").unwrap_or(raw);
            if state == DocLineState::Normal {
                let stripped = doc_line.trim();
                if let Some(handled) = apply_marker(
                    stripped,
                    &mut result.is_deprecated,
                    &mut result.deprecated_message,
                    &mut result.is_experimental,
                    &mut result.experimental_message,
                ) {
                    if handled {
                        continue;
                    }
                }
            }
            process_doc_line(
                doc_line,
                &mut result.description,
                &space_prefix,
                &mut state,
                self.dialect,
            );
        }
        (result, start)
    }

    /// `parse_class_doc_comment` (`gdscript_parser.cpp:4089`).
    fn parse_class_doc(&mut self, p_line: u32, single_line: bool) -> (ClassDoc, u32) {
        let (start, space_prefix) = self.block_start(p_line, single_line);
        let mut state = DocLineState::Normal;
        let mut in_brief = true;
        let mut result = ClassDoc::default();
        for line in start..=p_line {
            let Some(data) = self.comments.get(&line) else {
                continue;
            };
            let raw = self.comment_text(data);
            let doc_line = raw.strip_prefix("##").unwrap_or(raw);
            if state == DocLineState::Normal {
                let stripped = doc_line.trim();
                // A blank line separates the brief from the description (`:4124`).
                if in_brief && !result.brief.is_empty() && stripped.is_empty() {
                    in_brief = false;
                    continue;
                }
                if let Some(rest) = stripped.strip_prefix("@tutorial") {
                    if let Some((title, link)) = parse_tutorial(rest) {
                        result.tutorials.push((title, link));
                    }
                    // Invalid @tutorial syntax is skipped entirely, like upstream.
                    continue;
                }
                if let Some(handled) = apply_marker(
                    stripped,
                    &mut result.is_deprecated,
                    &mut result.deprecated_message,
                    &mut result.is_experimental,
                    &mut result.experimental_message,
                ) {
                    if handled {
                        continue;
                    }
                }
            }
            let target = if in_brief {
                &mut result.brief
            } else {
                &mut result.description
            };
            process_doc_line(doc_line, target, &space_prefix, &mut state, self.dialect);
        }
        (result, start)
    }
}

/// The shared `@deprecated[: msg]` / `@experimental[: msg]` recognition (`:4067-4080`).
/// `Some(true)` = the line was a marker and is consumed; `Some(false)` = not a marker.
#[allow(clippy::unnecessary_wraps)]
fn apply_marker(
    stripped: &str,
    is_deprecated: &mut bool,
    deprecated_message: &mut String,
    is_experimental: &mut bool,
    experimental_message: &mut String,
) -> Option<bool> {
    if stripped == "@deprecated" || stripped.starts_with("@deprecated:") {
        *is_deprecated = true;
        if let Some(msg) = stripped.strip_prefix("@deprecated:") {
            *deprecated_message = msg.trim().to_string();
        }
        return Some(true);
    }
    if stripped == "@experimental" || stripped.starts_with("@experimental:") {
        *is_experimental = true;
        if let Some(msg) = stripped.strip_prefix("@experimental:") {
            *experimental_message = msg.trim().to_string();
        }
        return Some(true);
    }
    Some(false)
}

/// `@tutorial[(Title)]: url` (`gdscript_parser.cpp:4126-4170`); `rest` is everything after the
/// literal `@tutorial`. `None` = invalid syntax (skipped, like upstream).
fn parse_tutorial(rest: &str) -> Option<(String, String)> {
    let rest_trimmed_start = rest;
    if rest_trimmed_start.is_empty() {
        return None;
    }
    if let Some(link) = rest_trimmed_start.strip_prefix(':') {
        return Some((String::new(), link.trim().to_string()));
    }
    // `@tutorial ( The Title ) : url`
    let after_ws = rest_trimmed_start.trim_start_matches([' ', '\t']);
    let inner = after_ws.strip_prefix('(')?;
    let close = inner.find(')')?;
    let title = inner[..close].trim().to_string();
    let after_close = inner[close + 1..].trim_start_matches([' ', '\t']);
    let link = after_close.strip_prefix(':')?;
    Some((title, link.trim().to_string()))
}

/// `_process_doc_line` (`gdscript_parser.cpp:3905-4020`) — joins this line onto the
/// accumulated `text` (space join in normal prose, no join after `[br]`, newline joins around
/// fences) while tracking the opaque-content state machine for `[code]`/`[codeblock]`/`[kbd]`.
fn process_doc_line(
    p_line: &str,
    text: &mut String,
    space_prefix: &str,
    state: &mut DocLineState,
    dialect: Dialect,
) {
    // DIALECT(4.7): gdscript_parser.cpp _process_doc_line() — `strip_edges` became
    // `lstrip(" \t")` / `rstrip(" \t")`, so only spaces and tabs are trimmed. A `\r` left by CRLF
    // handling now survives into the doc text instead of being silently eaten.
    let owned_line: String;
    let line: &str = if *state == DocLineState::Normal {
        if dialect < Dialect::Godot4_7 {
            p_line.trim_start()
        } else {
            p_line.trim_start_matches([' ', '\t'])
        }
    } else {
        p_line.strip_prefix(space_prefix).unwrap_or(p_line)
    };

    let mut line_join = "";
    if !text.is_empty() {
        if *state == DocLineState::Normal {
            if text.ends_with("[/codeblock]") {
                line_join = "\n";
            } else if text.ends_with("[br]") {
                // DIALECT(4.7): a `[br]` ending the previous line and a `[br]` opening this one
                // together mean a paragraph break. The trailing `[br]` is moved off the
                // accumulator and onto the front of this line so the `[br][br]` pair meets in one
                // string, where the tag scan below turns it into a newline.
                if dialect >= Dialect::Godot4_7 {
                    text.truncate(text.len() - "[br]".len());
                    owned_line = format!("[br]{line}");
                    return process_doc_line_inner(&owned_line, text, state, dialect, "");
                }
            } else if text.ends_with('\n') {
                // DIALECT(4.7): gdscript_parser.cpp _process_doc_line() — 4.7 also refuses the
                // space join when the accumulator already ends in a newline, which is exactly what
                // its own `[br][br]` paragraph break leaves behind; without it every paragraph
                // after the first opens with a stray space. 4.6 has no paragraph break to protect,
                // and joined unconditionally.
                if dialect < Dialect::Godot4_7 {
                    line_join = " ";
                }
            } else {
                line_join = " ";
            }
        } else {
            line_join = "\n";
        }
    }
    process_doc_line_inner(line, text, state, dialect, line_join);
}

/// The tag-scanning half of [`process_doc_line`], split out so the 4.7 `[br][br]` path can
/// re-enter it with a rewritten line and no join.
fn process_doc_line_inner(
    line: &str,
    text: &mut String,
    state: &mut DocLineState,
    dialect: Dialect,
    line_join: &str,
) {
    let mut line_join = line_join.to_string();

    let mut result = String::new();
    let mut from = 0usize;
    let mut buffer_start = 0usize;
    let len = line.len();
    loop {
        match *state {
            DocLineState::Normal => {
                let Some(lb_rel) = line[from..].find('[') else {
                    break;
                };
                let lb_pos = from + lb_rel;
                let Some(rb_rel) = line[lb_pos + 1..].find(']') else {
                    break;
                };
                let rb_pos = lb_pos + 1 + rb_rel;
                from = rb_pos + 1;
                let tag = &line[lb_pos + 1..rb_pos];
                // DIALECT(4.7): `[br][br]` collapses to a real paragraph break.
                if dialect >= Dialect::Godot4_7 && tag == "br" {
                    if line[from..].starts_with("[br]") {
                        result.push_str(&line[buffer_start..lb_pos]);
                        result.push('\n');
                        from += "[br]".len();
                        buffer_start = from;
                    }
                } else if tag == "code" || tag.starts_with("code ") {
                    *state = DocLineState::InCode;
                } else if tag == "codeblock" || tag.starts_with("codeblock ") {
                    if lb_pos == 0 {
                        line_join = "\n".to_string();
                    } else {
                        result.push_str(&line[buffer_start..lb_pos]);
                        result.push('\n');
                    }
                    result.push('[');
                    result.push_str(tag);
                    result.push(']');
                    if from < len {
                        result.push('\n');
                    }
                    *state = DocLineState::InCodeblock;
                    buffer_start = from;
                } else if tag == "kbd" {
                    *state = DocLineState::InKbd;
                }
            }
            DocLineState::InCode => {
                let Some(rel) = line[from..].find("[/code]") else {
                    break;
                };
                from = from + rel + "[/code]".len();
                *state = DocLineState::Normal;
            }
            DocLineState::InCodeblock => {
                let Some(rel) = line[from..].find("[/codeblock]") else {
                    break;
                };
                let pos = from + rel;
                from = pos + "[/codeblock]".len();
                if pos == 0 {
                    line_join = "\n".to_string();
                } else {
                    result.push_str(&line[buffer_start..pos]);
                    result.push('\n');
                }
                result.push_str("[/codeblock]");
                if from < len {
                    result.push('\n');
                }
                *state = DocLineState::Normal;
                buffer_start = from;
            }
            DocLineState::InKbd => {
                let Some(rel) = line[from..].find("[/kbd]") else {
                    break;
                };
                from = from + rel + "[/kbd]".len();
                *state = DocLineState::Normal;
            }
        }
    }

    result.push_str(&line[buffer_start..]);
    let mut out = result;
    if *state == DocLineState::Normal {
        // DIALECT(4.7): the `rstrip(" \t")` half of the `strip_edges` change above.
        let kept = if dialect < Dialect::Godot4_7 {
            out.trim_end().len()
        } else {
            out.trim_end_matches([' ', '\t']).len()
        };
        out.truncate(kept);
    }
    text.push_str(&line_join);
    text.push_str(&out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Member;

    fn docs(source: &str) -> (crate::ParseResult, DocTable) {
        docs_in(source, Dialect::DEFAULT)
    }

    fn docs_in(source: &str, dialect: Dialect) -> (crate::ParseResult, DocTable) {
        let result = crate::parse_with_options(
            source,
            &crate::ParseOptions {
                dialect,
                ..Default::default()
            },
        );
        let table = result.tree.docs.clone();
        (result, table)
    }

    /// The first member's description under one dialect.
    fn member_desc(source: &str, dialect: Dialect) -> String {
        let (r, t) = docs_in(source, dialect);
        t.member_docs[&member_id(&r, 0)].description.clone()
    }

    /// The id of the head class's `index`-th member (panics on classless trees).
    fn member_id(result: &crate::ParseResult, index: usize) -> NodeId {
        let root = result.tree.root_id().expect("root");
        let NodeKind::Class(class) = &result.tree.get(root).kind else {
            panic!("root is a class")
        };
        match &class.members[index] {
            Member::Class(id)
            | Member::Constant(id)
            | Member::Function(id)
            | Member::Signal(id)
            | Member::Variable(id)
            | Member::Enum(id)
            | Member::Group(id) => *id,
            Member::EnumValue(_) => panic!("unnamed enum value"),
        }
    }

    #[test]
    fn block_doc_above_member() {
        let (r, t) = docs(
            "extends Node\n## Speed of the unit.\n## In pixels per second.\nvar speed := 1.0\n",
        );
        let doc = &t.member_docs[&member_id(&r, 0)];
        assert_eq!(doc.description, "Speed of the unit. In pixels per second.");
    }

    #[test]
    fn inline_doc_on_member_line() {
        let (r, t) = docs("extends Node\nvar hp := 10 ## Health points.\n");
        let doc = &t.member_docs[&member_id(&r, 0)];
        assert_eq!(doc.description, "Health points.");
    }

    #[test]
    fn plain_comments_are_not_docs() {
        let (r, t) = docs("extends Node\n# not a doc\nvar x := 1\n");
        assert!(!t.member_docs.contains_key(&member_id(&r, 0)));
        assert!(t.class_docs.is_empty());
    }

    #[test]
    fn annotation_hoisting_pulls_the_block_above_annotations() {
        let (r, t) = docs("extends Node\n## Exported speed.\n@export\nvar speed := 1.0\n");
        let doc = &t.member_docs[&member_id(&r, 0)];
        assert_eq!(doc.description, "Exported speed.");
    }

    #[test]
    fn min_member_line_keeps_blocks_exclusive() {
        // The ## sits directly under `a`'s declaration end — `min_member_doc_line` blocks `b`
        // from claiming anything at or above a's end, and the block belongs to b only when it
        // starts strictly below.
        let (r, t) = docs("extends Node\nvar a := 1\n## For b.\nvar b := 2\n");
        assert!(!t.member_docs.contains_key(&member_id(&r, 0)));
        assert_eq!(t.member_docs[&member_id(&r, 1)].description, "For b.");
    }

    #[test]
    fn trailing_inline_comment_is_not_the_next_members_block() {
        // `new_line == false` for a trailing comment after code — never a block doc.
        let (r, t) = docs("extends Node\nvar a := 1 ## a's own inline doc\nvar b := 2\n");
        assert_eq!(
            t.member_docs[&member_id(&r, 0)].description,
            "a's own inline doc"
        );
        assert!(!t.member_docs.contains_key(&member_id(&r, 1)));
    }

    #[test]
    fn head_class_doc_brief_description_and_markers() {
        let src = "\
## Brief line one.
## Brief line two.
##
## Long description here.
## @tutorial(Guide): https://example.com/guide
## @tutorial: https://example.com/bare
## @deprecated: Use OtherClass instead.
## @experimental
class_name Documented
extends Node

var member := 1
";
        let (r, t) = docs(src);
        let root = r.tree.root_id().unwrap();
        let doc = &t.class_docs[&root];
        assert_eq!(doc.brief, "Brief line one. Brief line two.");
        assert_eq!(doc.description, "Long description here.");
        assert_eq!(
            doc.tutorials,
            vec![
                ("Guide".to_string(), "https://example.com/guide".to_string()),
                (String::new(), "https://example.com/bare".to_string()),
            ]
        );
        assert!(doc.is_deprecated);
        assert_eq!(doc.deprecated_message, "Use OtherClass instead.");
        assert!(doc.is_experimental);
        assert_eq!(doc.experimental_message, "");
    }

    #[test]
    fn member_doc_above_first_member_is_not_the_class_doc() {
        // `max_script_doc_line` clamps to just above the member's block, so the head scan
        // (which runs last) never re-reads it.
        let (r, t) = docs("extends Node\n\n## Belongs to speed.\nvar speed := 1.0\n");
        let root = r.tree.root_id().unwrap();
        assert!(!t.class_docs.contains_key(&root));
        assert_eq!(
            t.member_docs[&member_id(&r, 0)].description,
            "Belongs to speed."
        );
    }

    #[test]
    fn codeblock_lines_join_with_newlines_and_br_suppresses_the_space() {
        let src = "\
extends Node
## Example:
## [codeblock]
## var x = 1
## var y = 2
## [/codeblock]
## After.[br]
## No space before me.
func f():
\tpass
";
        let (r, t) = docs(src);
        let doc = &t.member_docs[&member_id(&r, 0)];
        assert_eq!(
            doc.description,
            "Example:\n[codeblock]\nvar x = 1\nvar y = 2\n[/codeblock]\nAfter.[br]No space before me."
        );
    }

    #[test]
    fn codeblock_preserves_indentation_via_space_prefix() {
        let src = "\
extends Node
## [codeblock]
## if x:
##     nested()
## [/codeblock]
func f():
\tpass
";
        let (r, t) = docs(src);
        let doc = &t.member_docs[&member_id(&r, 0)];
        // The leading newline is upstream parity: Godot's codeblock arm sets the line join to
        // a newline even on the block's first content line (`_process_doc_line`, `:3950`).
        // Downstream consumers (the gd_server converter) trim the prose edges.
        assert_eq!(
            doc.description,
            "\n[codeblock]\nif x:\n    nested()\n[/codeblock]"
        );
    }

    #[test]
    fn code_span_content_is_opaque() {
        // A `[code]` span containing `[i]` must not flip any state or get eaten.
        let (r, t) = docs("extends Node\n## Use [code][i]raw[/i][/code] here.\nvar x := 1\n");
        let doc = &t.member_docs[&member_id(&r, 0)];
        assert_eq!(doc.description, "Use [code][i]raw[/i][/code] here.");
    }

    #[test]
    fn enum_value_docs_block_inline_and_exclusivity() {
        let src = "\
extends Node
enum State {
\t## Standing still.
\tIDLE,
\tRUN, ## Moving fast.
\tJUMP,
}
";
        let (r, t) = docs(src);
        let enum_id = member_id(&r, 0);
        assert_eq!(
            t.enum_value_docs[&(enum_id, 0)].description,
            "Standing still."
        );
        assert_eq!(t.enum_value_docs[&(enum_id, 1)].description, "Moving fast.");
        assert!(!t.enum_value_docs.contains_key(&(enum_id, 2)));
    }

    #[test]
    fn inner_class_doc_attaches_and_member_docs_stay_scoped() {
        let src = "\
extends Node

## The inner helper.
class Inner:
\t## Inner member.
\tvar x := 1
";
        let (r, t) = docs(src);
        let inner_id = member_id(&r, 0);
        assert_eq!(t.class_docs[&inner_id].brief, "The inner helper.");
        let NodeKind::Class(inner) = &r.tree.get(inner_id).kind else {
            panic!("inner class");
        };
        let Member::Variable(x_id) = inner.members[0] else {
            panic!("inner var");
        };
        assert_eq!(t.member_docs[&x_id].description, "Inner member.");
    }

    #[test]
    fn comment_only_or_empty_sources_produce_empty_tables() {
        for d in [Dialect::Godot4_6, Dialect::Godot4_7] {
            for src in ["", "# plain\n# comments\n"] {
                let (_r, t) = docs_in(src, d);
                assert!(t.is_empty(), "source {src:?} must yield no docs at {d}");
            }
        }
    }

    /// DIALECT(4.7): a file that is *nothing but* a `##` run. The head-class doc scan runs up to
    /// `head->end_line` when the class has no members (`gdscript_parser.cpp:845`), and that line
    /// comes from the parser's `previous` token — which 4.7 default-constructs at line 1 instead
    /// of line 0 (`gdscript_tokenizer.h`, see `empty_token`). So the scan reaches line 1 and picks
    /// the comment up at 4.7, where 4.6 stopped one line short of it.
    #[test]
    fn a_file_of_only_doc_comments_attaches_them_to_the_head_class_only_at_4_7() {
        let src = "## floating doc\n";
        assert!(docs_in(src, Dialect::Godot4_6).1.is_empty());
        let (_r, t) = docs_in(src, Dialect::Godot4_7);
        let root = crate::parse(src).tree.root_id().expect("root");
        assert_eq!(t.class_docs[&root].brief, "floating doc");
    }

    // ===============================================================================================
    // The 4.7 `_process_doc_line` delta: `strip_edges` → `lstrip`/`rstrip(" \t")`, and `[br][br]`
    // as a paragraph break. User-visible in hover, and no diagnostics gate would catch a regression.
    // ===============================================================================================

    #[test]
    fn br_br_is_a_paragraph_break_only_at_4_7() {
        // A `[br][br]` pair inside one line.
        let src = "extends Node\n## First.[br][br]Second.\nvar x := 1\n";
        assert_eq!(member_desc(src, Dialect::Godot4_6), "First.[br][br]Second.");
        assert_eq!(member_desc(src, Dialect::Godot4_7), "First.\nSecond.");
    }

    #[test]
    fn br_br_spanning_two_doc_lines_is_a_paragraph_break_at_4_7() {
        // The pair meets across the line join: a trailing `[br]` plus a leading `[br]`.
        let src = "extends Node\n## First.[br]\n## [br]Second.\nvar x := 1\n";
        assert_eq!(member_desc(src, Dialect::Godot4_6), "First.[br][br]Second.");
        assert_eq!(member_desc(src, Dialect::Godot4_7), "First.\nSecond.");
    }

    /// The paragraph break is produced by the END of one doc line, so the NEXT line joins onto a
    /// string that already ends in `\n`. 4.7 refuses the space join there
    /// (`!r_text.ends_with("\n")`, gdscript_parser.cpp); without that the second paragraph opened
    /// with a stray space.
    #[test]
    fn a_paragraph_break_at_a_line_end_joins_the_next_line_without_a_space_at_4_7() {
        let src = "extends Node\n## First.[br][br]\n## Second.\nvar x := 1\n";
        assert_eq!(member_desc(src, Dialect::Godot4_6), "First.[br][br]Second.");
        assert_eq!(member_desc(src, Dialect::Godot4_7), "First.\nSecond.");
    }

    #[test]
    fn a_lone_br_is_untouched_in_both_dialects() {
        let src = "extends Node\n## First.[br]Second.\nvar x := 1\n";
        for d in [Dialect::Godot4_6, Dialect::Godot4_7] {
            assert_eq!(member_desc(src, d), "First.[br]Second.", "dialect {d:?}");
        }
    }

    #[test]
    fn a_carriage_return_survives_the_trim_only_at_4_7() {
        // CRLF leaves a `\r` at the end of the doc line. 4.6's `strip_edges` ate it; 4.7's
        // `rstrip(" \t")` does not.
        let src = "extends Node\r\n## Speed.\r\nvar x := 1\r\n";
        assert_eq!(member_desc(src, Dialect::Godot4_6), "Speed.");
        assert_eq!(member_desc(src, Dialect::Godot4_7), "Speed.\r");
    }
}
