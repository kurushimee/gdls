//! M7 (#62) — the one BBCode→prose converter for every outgoing doc string (anti-catalog W8:
//! raw BBCode must never reach the wire). Both prose sources are BBCode-flavored — GDScript
//! `##` doc comments (`gd_syntax::doc_comments`) and `extension_api.json` descriptions — so this
//! single module serves hover today and completion/signatureHelp documentation in M8.
//!
//! Output is GitHub-Flavored Markdown, or plaintext (markup stripped) for clients whose
//! `hover.contentFormat` prefers it. Cross-references (`[method X]`, `[member X]`, …) render as
//! code spans in M7; linking into the materialized native stubs is an M8 follow-up (the
//! converter would grow an optional resolver hook — materializing a stub is a disk write, not a
//! hover-time side effect to take lightly).
//!
//! Tag inventory: every tag observed in the stock 4.6.3 class docs is handled or deliberately
//! stripped; the `embedded_dump_tag_inventory_is_covered` test harvests the embedded dump's
//! actual tag set and fails when a future dump introduces one this table doesn't know.

/// Which prose flavor the client negotiated (`ClientCaps::hover_format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ProseFormat {
    /// GitHub-Flavored Markdown. Sent only when the client ASKED for it.
    Markdown,
    /// The floor (#261). A client that advertised no `contentFormat` / `documentationFormat` has
    /// told the server nothing about what it can render, and `MarkupKind::PlainText` is the one
    /// thing every client understands — sending markdown on that assumption surfaces raw
    /// ``` fences and `**` in a popup that cannot render them. Every captured editor profile
    /// (vscode, neovim, helix, zed, eglot, sublime) declares markdown explicitly, so this default
    /// is reached only by a genuinely minimal client. Completion and signatureHelp already took
    /// this floor via an explicit `unwrap_or`; making it the derived default puts hover on the
    /// same footing and keeps the three from drifting apart again.
    #[default]
    PlainText,
}

/// Convert one BBCode-flavored doc string. Never panics; unknown tags pass through verbatim
/// (markdown) or are stripped conservatively (plaintext) — never an error.
pub(crate) fn bbcode_to(format: ProseFormat, input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    let mut rest = input;
    let md = format == ProseFormat::Markdown;
    while let Some(lb) = rest.find('[') {
        out.push_str(&rest[..lb]);
        let after = &rest[lb + 1..];
        let Some(rb) = after.find(']') else {
            // Unterminated bracket — emit the rest verbatim.
            out.push_str(&rest[lb..]);
            rest = "";
            break;
        };
        let tag = &after[..rb];
        rest = &after[rb + 1..];
        match classify(tag) {
            Tag::Bold => out.push_str(if md { "**" } else { "" }),
            Tag::Italic => out.push_str(if md { "*" } else { "" }),
            Tag::Strike => out.push_str(if md { "~~" } else { "" }),
            Tag::StripKeepContent => {}
            Tag::Code => {
                // Opaque content until the closer — inner `[` must not re-enter the scanner.
                let (content, after_close) = split_until(rest, "[/code]");
                if md {
                    out.push('`');
                    out.push_str(content);
                    out.push('`');
                } else {
                    out.push_str(content);
                }
                rest = after_close;
            }
            Tag::Kbd => {
                let (content, after_close) = split_until(rest, "[/kbd]");
                if md {
                    out.push('`');
                    out.push_str(content);
                    out.push('`');
                } else {
                    out.push_str(content);
                }
                rest = after_close;
            }
            Tag::Codeblock => {
                let (content, after_close) = split_until(rest, "[/codeblock]");
                push_fence(&mut out, md, content);
                rest = after_close;
            }
            Tag::Codeblocks => {
                // Dual-language example: keep only the [gdscript] variant, drop [csharp].
                let (content, after_close) = split_until(rest, "[/codeblocks]");
                if let Some(gd_open) = content.find("[gdscript]") {
                    let gd = &content[gd_open + "[gdscript]".len()..];
                    let gd = gd.split("[/gdscript]").next().unwrap_or(gd);
                    push_fence(&mut out, md, gd);
                }
                rest = after_close;
            }
            Tag::Img => {
                let (_, after_close) = split_until(rest, "[/img]");
                rest = after_close;
            }
            Tag::MethodRef(name) => {
                if md {
                    out.push('`');
                    out.push_str(name);
                    out.push_str("()`");
                } else {
                    out.push_str(name);
                    out.push_str("()");
                }
            }
            Tag::SymbolRef(name) => {
                if md {
                    out.push('`');
                    out.push_str(name);
                    out.push('`');
                } else {
                    out.push_str(name);
                }
            }
            Tag::UrlBare => {
                let (link, after_close) = split_until(rest, "[/url]");
                if md {
                    out.push('<');
                    out.push_str(link.trim());
                    out.push('>');
                } else {
                    out.push_str(link.trim());
                }
                rest = after_close;
            }
            Tag::UrlTitled(link) => {
                let (title, after_close) = split_until(rest, "[/url]");
                rest = after_close;
                if md {
                    out.push('[');
                    out.push_str(title);
                    out.push_str("](");
                    out.push_str(link);
                    out.push(')');
                } else {
                    out.push_str(title);
                    out.push_str(" (");
                    out.push_str(link);
                    out.push(')');
                }
            }
            Tag::Br => out.push_str(if md { "  \n" } else { "\n" }),
            Tag::Lb => out.push('['),
            Tag::Rb => out.push(']'),
            Tag::Unknown => {
                // Leave verbatim in markdown (never destroy content we don't understand); strip
                // in plaintext (the conservative downgrade).
                if md {
                    out.push('[');
                    out.push_str(tag);
                    out.push(']');
                } else {
                    log::debug!("bbcode: stripping unknown tag [{tag}] in plaintext mode");
                }
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Append a converted doc paragraph below a signature block, rust-analyzer style: a `---` rule
/// between fence and prose in markdown, a blank line in plaintext. Empty prose appends nothing.
pub(crate) fn append_doc(out: &mut String, format: ProseFormat, bbcode: &str) {
    let prose = bbcode_to(format, bbcode);
    if prose.is_empty() {
        return;
    }
    if format == ProseFormat::Markdown {
        if !out.contains("\n---\n") {
            out.push_str("\n\n---\n");
        }
        out.push('\n');
        out.push_str(&prose);
    } else {
        out.push_str("\n\n");
        out.push_str(&prose);
    }
}

/// Downgrade an assembled markdown hover body for a plaintext-only client: code fences and
/// `---` rules drop, inline code/bold/italic markers strip, text survives. Deliberately a
/// simplifier, not a markdown parser — hover bodies are assembled in-house and use a known
/// subset.
pub(crate) fn markdown_to_plaintext(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    for line in md.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            continue;
        }
        if trimmed == "---" {
            continue;
        }
        let mut cleaned = line.replace("**", "");
        cleaned = cleaned.replace('`', "");
        out.push_str(&cleaned);
        out.push('\n');
    }
    out.trim().to_string()
}

enum Tag<'a> {
    Bold,
    Italic,
    Strike,
    /// Presentational tags whose content stays: `[u]`, `[center]`, `[color=…]`, `[font=…]`,
    /// and every closer of a strip-tag.
    StripKeepContent,
    Code,
    Kbd,
    Codeblock,
    Codeblocks,
    Img,
    /// `[method X]` / `[constructor X]` / `[operator X]` → `X()`.
    MethodRef(&'a str),
    /// `[member X]`, `[signal X]`, `[constant X]`, `[enum X]`, `[annotation @X]`,
    /// `[theme_item X]`, `[param x]`, and bare `[ClassName]` → code span.
    SymbolRef(&'a str),
    UrlBare,
    UrlTitled(&'a str),
    Br,
    Lb,
    Rb,
    Unknown,
}

fn classify(tag: &str) -> Tag<'_> {
    match tag {
        "b" | "/b" => Tag::Bold,
        "i" | "/i" => Tag::Italic,
        "s" | "/s" => Tag::Strike,
        "u" | "/u" | "center" | "/center" | "/color" | "/font" | "/bgcolor" | "/fgcolor" => {
            Tag::StripKeepContent
        }
        // Closers of opaque-content tags never reach the scanner in well-formed input (their
        // openers consume up to and including them); a stray unmatched one in malformed docs
        // strips rather than printing raw. Bare [gdscript]/[csharp] only occur inside
        // [codeblocks] (also consumed opaquely) — same defensive rule.
        "/code" | "/codeblock" | "/codeblocks" | "/kbd" | "/url" | "/img" | "gdscript"
        | "/gdscript" | "csharp" | "/csharp" => Tag::StripKeepContent,
        "kbd" => Tag::Kbd,
        "codeblocks" => Tag::Codeblocks,
        "img" => Tag::Img,
        "url" => Tag::UrlBare,
        "br" => Tag::Br,
        "lb" => Tag::Lb,
        "rb" => Tag::Rb,
        _ => {
            if tag == "code" || tag.starts_with("code ") {
                return Tag::Code;
            }
            if tag == "codeblock" || tag.starts_with("codeblock ") {
                return Tag::Codeblock;
            }
            if let Some(rest) = tag.strip_prefix("url=") {
                return Tag::UrlTitled(rest);
            }
            for prefix in ["method ", "constructor ", "operator "] {
                if let Some(name) = tag.strip_prefix(prefix) {
                    return Tag::MethodRef(name.trim());
                }
            }
            for prefix in [
                "member ",
                "signal ",
                "constant ",
                "enum ",
                "annotation ",
                "theme_item ",
                "param ",
            ] {
                if let Some(name) = tag.strip_prefix(prefix) {
                    return Tag::SymbolRef(name.trim());
                }
            }
            if tag.starts_with("color=")
                || tag.starts_with("font=")
                || tag.starts_with("bgcolor=")
                || tag.starts_with("fgcolor=")
            {
                return Tag::StripKeepContent;
            }
            // Bare class reference: identifier-shaped, starting uppercase or `@` (`[Node2D]`,
            // `[@GlobalScope]`).
            let mut chars = tag.chars();
            if let Some(first) = chars.next() {
                if (first.is_ascii_uppercase() || first == '@')
                    && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
                {
                    return Tag::SymbolRef(tag);
                }
            }
            Tag::Unknown
        }
    }
}

/// Split `rest` at the first occurrence of `closer`, returning `(content, after_closer)`.
/// A missing closer treats the remainder as content (degrade, never lose text).
fn split_until<'a>(rest: &'a str, closer: &str) -> (&'a str, &'a str) {
    match rest.find(closer) {
        Some(pos) => (&rest[..pos], &rest[pos + closer.len()..]),
        None => (rest, ""),
    }
}

/// Emit codeblock content as a gdscript fence (markdown) or an indented-as-is block (plaintext).
fn push_fence(out: &mut String, md: bool, content: &str) {
    let body = content.trim_matches('\n');
    if md {
        out.push_str("```gdscript\n");
        out.push_str(body);
        out.push_str("\n```");
    } else {
        out.push_str(body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(input: &str) -> String {
        bbcode_to(ProseFormat::Markdown, input)
    }
    fn plain(input: &str) -> String {
        bbcode_to(ProseFormat::PlainText, input)
    }

    #[test]
    fn inline_styles_and_refs() {
        assert_eq!(
            md("Call [method queue_free] on [b]every[/b] [member owner] of [Node2D]."),
            "Call `queue_free()` on **every** `owner` of `Node2D`."
        );
        assert_eq!(
            plain("Call [method queue_free] on [b]every[/b] [member owner]."),
            "Call queue_free() on every owner."
        );
    }

    #[test]
    fn code_content_is_opaque() {
        assert_eq!(
            md("Use [code]arr[0][/code] and [code skip-lint]x [b] y[/code]."),
            "Use `arr[0]` and `x [b] y`."
        );
    }

    #[test]
    fn codeblock_renders_a_gdscript_fence() {
        assert_eq!(
            md("Example:\n[codeblock]\nvar x = 1\nif x:\n    pass\n[/codeblock]\nDone."),
            "Example:\n```gdscript\nvar x = 1\nif x:\n    pass\n```\nDone."
        );
    }

    #[test]
    fn codeblocks_keeps_only_the_gdscript_variant() {
        let input = "[codeblocks]\n[gdscript]\nvar a = 1\n[/gdscript]\n[csharp]\nint a = 1;\n[/csharp]\n[/codeblocks]";
        assert_eq!(md(input), "```gdscript\nvar a = 1\n```");
    }

    #[test]
    fn urls_render_as_links() {
        assert_eq!(
            md("See [url=https://example.com]the docs[/url] or [url]https://godotengine.org[/url]."),
            "See [the docs](https://example.com) or <https://godotengine.org>."
        );
        assert_eq!(
            plain("See [url=https://example.com]the docs[/url]."),
            "See the docs (https://example.com)."
        );
    }

    #[test]
    fn br_lb_rb_and_presentational_tags() {
        assert_eq!(md("a[br]b"), "a  \nb");
        assert_eq!(md("array[lb]0[rb]"), "array[0]");
        assert_eq!(md("[u]under[/u] [color=red]red[/color]"), "under red");
        assert_eq!(md("strike [s]this[/s]"), "strike ~~this~~");
    }

    #[test]
    fn unknown_tags_pass_through_in_markdown() {
        assert_eq!(md("a [weird-tag] b"), "a [weird-tag] b");
        assert_eq!(plain("a [weird-tag] b"), "a  b");
    }

    #[test]
    fn append_doc_uses_the_rust_analyzer_separator() {
        let mut out = "```gdscript\nfunc f()\n```".to_string();
        append_doc(&mut out, ProseFormat::Markdown, "Does the [b]thing[/b].");
        assert_eq!(
            out,
            "```gdscript\nfunc f()\n```\n\n---\n\nDoes the **thing**."
        );
        // A second paragraph joins below without a second rule.
        append_doc(&mut out, ProseFormat::Markdown, "More.");
        assert!(out.ends_with("Does the **thing**.\nMore."));
    }

    #[test]
    fn markdown_to_plaintext_strips_fences_rules_and_markers() {
        let input = "```gdscript\nfunc f() -> void\n```\n\n---\n\nDoes the **thing** with `x`.";
        assert_eq!(
            markdown_to_plaintext(input),
            "func f() -> void\n\n\nDoes the thing with x."
        );
    }

    /// The "tested against the stock 4.6.3 tag set" acceptance. The embedded dump is the
    /// no-docs variant, so the inventory was harvested ONCE from the full 4.6.3 class
    /// reference (`doc/classes/*.xml` + module doc_classes, every `[tag]` head with ≥5 uses)
    /// and pinned here: every structural tag must classify as something the converter handles,
    /// and bare class references must hit the code-span arm. A future Godot doc tag lands by
    /// extending the fixture + the classifier together.
    #[test]
    fn stock_463_tag_inventory_is_covered() {
        let structural = [
            "b",
            "/b",
            "i",
            "/i",
            "s",
            "/s",
            "u",
            "/u",
            "code",
            "code skip-lint",
            "/code",
            "codeblock",
            "codeblock lang=text",
            "/codeblock",
            "codeblocks",
            "/codeblocks",
            "kbd",
            "/kbd",
            "url",
            "url=https://example.com",
            "/url",
            "br",
            "lb",
            "rb",
            "center",
            "/center",
            "color=red",
            "/color",
            "font=res://f.ttf",
            "/font",
            "img",
            "/img",
            "method foo",
            "constructor Color",
            "operator *",
            "member size",
            "signal pressed",
            "constant KEY_A",
            "enum Mode",
            "annotation @export",
            "theme_item font_color",
            "param index",
        ];
        for tag in structural {
            assert!(
                !matches!(classify(tag), Tag::Unknown),
                "structural stock tag [{tag}] is unhandled"
            );
        }
        for class_ref in [
            "Node2D",
            "RID",
            "@GlobalScope",
            "PackedByteArray",
            "Vector3",
        ] {
            assert!(
                matches!(classify(class_ref), Tag::SymbolRef(_)),
                "bare class ref [{class_ref}] must render as a code span"
            );
        }
    }
}
