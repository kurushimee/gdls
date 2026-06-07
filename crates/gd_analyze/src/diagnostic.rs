//! The analyzer's diagnostic sink — the port of Godot's `push_error` / `push_warning` outputs.
//!
//! Errors and warnings carry a Godot code string (the warning name, or `"error"` for a bare
//! `push_error`) and a byte span. `gd_server` maps these to `lsp_types::Diagnostic` at the protocol
//! boundary (WP-G); the analyze-phase conformance harness compares them against the golden `.out`
//! files. Warning deferral and `@warning_ignore` filtering wrap this sink in the body pass (WP-C+).

use gd_syntax::ByteSpan;

use crate::warnings::{self, WarnLevel, WarningCode};

/// Diagnostic severity. Discriminants match `lsp_types::DiagnosticSeverity` so the server boundary is
/// a trivial cast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error = 1,
    Warning = 2,
}

/// One emitted diagnostic for a file. Fields are crate-private — construct through
/// [`DiagnosticSink::push_error`] / [`DiagnosticSink::push_error_with_line`] /
/// [`DiagnosticSink::push_warning`] (or [`Diagnostic::new_error`] / [`Diagnostic::new_warning`]
/// for sink-free construction), which enforce the severity ↔ `code` ↔ `warning_code` bond:
/// errors carry `code = "error"` and `warning_code = None` (except for the 4 error-by-default
/// warnings, which carry their warning code); warnings carry the matching `PNAME` string in
/// `code` plus `Some(warning_code)`. Read through the accessor methods below.
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub(crate) severity: Severity,
    /// Byte range into the source; converted to an LSP range at the protocol boundary.
    pub(crate) span: ByteSpan,
    /// Godot's warning name (e.g. `"UNUSED_VARIABLE"`), or `"error"` for a bare type/semantic error.
    pub(crate) code: String,
    /// The exact Godot message string.
    pub(crate) message: String,
    /// The originating warning code, if this is a warning (`None` for errors).
    pub(crate) warning_code: Option<WarningCode>,
    /// Optional explicit 1-based line override. When set, diagnostic renderers use this
    /// instead of deriving the line from [`Self::span`]. WP-R3 uses this for emissions that
    /// mirror Godot's null-source `push_error` path (gdscript_parser.cpp:241-244), where the
    /// Godot inherits the parser's `previous` token's line — for end-of-parse emissions, the
    /// synthetic post-EOF line stamped on [`gd_syntax::ParseTree::eof_line`]
    /// (e.g. `match_with_subscript.gd`'s subscript-Index pattern, analyzer.cpp:2466 with
    /// `expr == nullptr`).
    pub(crate) line: Option<u32>,
}

impl Diagnostic {
    /// Build a bare type/semantic error. Use [`Self::new_error_with_line`] for Godot's
    /// null-source `push_error` path (WP-R3 — see the [`line`](Self::line) field doc).
    pub fn new_error(span: ByteSpan, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            span,
            code: "error".to_owned(),
            message: message.into(),
            warning_code: None,
            line: None,
        }
    }

    /// Like [`Self::new_error`] but stamps an explicit 1-based line.
    pub fn new_error_with_line(span: ByteSpan, line: u32, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            span,
            code: "error".to_owned(),
            message: message.into(),
            warning_code: None,
            line: Some(line),
        }
    }

    /// Build a resolved warning. `severity` is the post-policy effective severity (so the 4
    /// error-by-default warnings produce `Severity::Error` here); `code` and `warning_code` are
    /// always bonded to the same [`WarningCode`].
    pub fn new_warning(
        severity: Severity,
        span: ByteSpan,
        code: WarningCode,
        message: impl Into<String>,
    ) -> Self {
        Diagnostic {
            severity,
            span,
            code: warnings::name_from_code(code).to_owned(),
            message: message.into(),
            warning_code: Some(code),
            line: None,
        }
    }

    /// Effective severity (`Error` for errors and for promoted error-by-default warnings;
    /// `Warning` for emitted warnings).
    pub fn severity(&self) -> Severity {
        self.severity
    }

    /// Source byte range. Converted to an LSP `Range` at the protocol boundary via
    /// `PositionMapper::span_to_range`.
    pub fn span(&self) -> ByteSpan {
        self.span
    }

    /// Diagnostic code: Godot's warning `PNAME` (e.g. `"UNUSED_VARIABLE"`) for warnings and
    /// promoted-error warnings, `"error"` for bare type/semantic errors.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Verbatim Godot message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Originating [`WarningCode`] when this is a warning (or a promoted error-by-default
    /// warning); `None` for bare type/semantic errors.
    pub fn warning_code(&self) -> Option<WarningCode> {
        self.warning_code
    }

    /// Optional explicit 1-based line override (WP-R3 / Godot null-source `push_error` path); see
    /// the [`line`](Self::line) field doc. Consumers that render the byte span (the LSP boundary
    /// in particular) should ignore this; consumers that produce `.out`-style line-number diff
    /// output (the conformance harness) should prefer it when `Some`.
    pub fn line(&self) -> Option<u32> {
        self.line
    }
}

/// Accumulates the diagnostics produced while analyzing one file.
#[derive(Debug, Default)]
pub struct DiagnosticSink {
    diagnostics: Vec<Diagnostic>,
}

impl DiagnosticSink {
    pub fn new() -> Self {
        DiagnosticSink::default()
    }

    /// Godot's `push_error`: an unconditional type/semantic error.
    pub fn push_error(&mut self, message: impl Into<String>, span: ByteSpan) {
        self.diagnostics.push(Diagnostic::new_error(span, message));
    }

    /// Variant of [`Self::push_error`] that stamps an explicit 1-based line on the diagnostic.
    /// Used by emission sites that mirror Godot's null-source `push_error` path
    /// (gdscript_parser.cpp:241-244, which reads `previous.start_line` / `previous.end_line`
    /// when the source node pointer is null). At end-of-parse the parser's `previous` token
    /// is at the synthetic post-EOF line — see [`gd_syntax::ParseTree::eof_line`].
    pub fn push_error_with_line(&mut self, message: impl Into<String>, span: ByteSpan, line: u32) {
        self.diagnostics
            .push(Diagnostic::new_error_with_line(span, line, message));
    }

    /// Emit a warning at the given resolved level. `Ignore` is dropped (returns `false`); `Warn` and
    /// `Error` produce a diagnostic with the verbatim Godot message (returns `true`).
    pub fn push_warning(
        &mut self,
        code: WarningCode,
        level: WarnLevel,
        symbols: &[String],
        span: ByteSpan,
    ) -> bool {
        let severity = match level {
            WarnLevel::Ignore => return false,
            WarnLevel::Warn => Severity::Warning,
            WarnLevel::Error => Severity::Error,
        };
        self.diagnostics.push(Diagnostic::new_warning(
            severity,
            span,
            code,
            warnings::format_warning(code, symbols),
        ));
        true
    }

    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Total emitted diagnostics so far — used by passes that want to gate a follow-up warning
    /// behind "didn't already emit anything for this expression" (e.g. Godot's
    /// `INFERENCE_ON_VARIANT` warning is suppressed when the initializer already emitted a cycle
    /// or Cannot-infer error — see resolve_assignable / cyclic_ref_var.gd).
    pub fn diagnostic_count(&self) -> usize {
        self.diagnostics.len()
    }

    /// Consume the sink, yielding the diagnostics in emission order. Godot's runner
    /// captures diagnostics in real-time during analysis so the `.out` files reflect the
    /// traversal sequence (interface-pass emissions before body-pass emissions, etc.).
    /// gdls's resolver mirrors that traversal sequence; preserving emission order matches
    /// Godot's golden files exactly without re-sorting.
    pub fn finish(self) -> Vec<Diagnostic> {
        self.diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> ByteSpan {
        ByteSpan { start: 0, end: 4 }
    }

    #[test]
    fn error_carries_message_and_code() {
        let mut sink = DiagnosticSink::new();
        sink.push_error("Cannot infer the type of \"x\".", span());
        assert!(sink.has_errors());
        let d = &sink.diagnostics()[0];
        assert_eq!(d.severity(), Severity::Error);
        assert_eq!(d.code(), "error");
        assert_eq!(d.warning_code(), None);
    }

    #[test]
    fn ignore_level_drops_the_warning() {
        let mut sink = DiagnosticSink::new();
        let emitted = sink.push_warning(
            WarningCode::UnusedVariable,
            WarnLevel::Ignore,
            &["x".to_owned()],
            span(),
        );
        assert!(!emitted);
        assert!(sink.is_empty());
    }

    #[test]
    fn error_by_default_warning_becomes_an_error() {
        let mut sink = DiagnosticSink::new();
        sink.push_warning(
            WarningCode::InferenceOnVariant,
            WarnLevel::Error,
            &["variable".to_owned()],
            span(),
        );
        let d = &sink.diagnostics()[0];
        assert_eq!(d.severity(), Severity::Error);
        assert_eq!(d.code(), "INFERENCE_ON_VARIANT");
        assert_eq!(d.warning_code(), Some(WarningCode::InferenceOnVariant));
        assert!(sink.has_errors());
    }

    #[test]
    fn finish_preserves_emission_order() {
        // WP-Q34: `finish` no longer sorts. Godot's runner captures diagnostics during
        // analysis (their `.out` golden files reflect traversal sequence), so emission
        // order is the source of truth — preserving it matches Godot without re-sorting.
        let mut sink = DiagnosticSink::new();
        sink.push_error("second", ByteSpan { start: 10, end: 12 });
        sink.push_error("first", ByteSpan { start: 2, end: 4 });
        let out = sink.finish();
        assert_eq!(out[0].message(), "second");
        assert_eq!(out[1].message(), "first");
    }

    #[test]
    fn explicit_line_override_round_trips() {
        // WP-R3 plumbing: push_error_with_line stamps the override; the accessor reads it.
        let mut sink = DiagnosticSink::new();
        sink.push_error_with_line("synthetic post-EOF emission", span(), 42);
        let d = &sink.diagnostics()[0];
        assert_eq!(d.line(), Some(42));
        // The plain push_error path stays None so the LSP boundary keeps rendering the span.
        sink.push_error("regular error", span());
        assert_eq!(sink.diagnostics()[1].line(), None);
    }

    #[test]
    fn diagnostic_bond_holds_through_constructors() {
        // Severity ↔ code ↔ warning_code bond: errors carry code="error" + warning_code=None;
        // warnings carry the PNAME + Some(code); promoted error-by-default warnings carry
        // Severity::Error but keep the warning code attached.
        let err = Diagnostic::new_error(span(), "x");
        assert_eq!(err.severity(), Severity::Error);
        assert_eq!(err.code(), "error");
        assert_eq!(err.warning_code(), None);

        let warn =
            Diagnostic::new_warning(Severity::Warning, span(), WarningCode::UnusedVariable, "y");
        assert_eq!(warn.severity(), Severity::Warning);
        assert_eq!(warn.code(), "UNUSED_VARIABLE");
        assert_eq!(warn.warning_code(), Some(WarningCode::UnusedVariable));

        let promoted = Diagnostic::new_warning(
            Severity::Error,
            span(),
            WarningCode::InferenceOnVariant,
            "z",
        );
        assert_eq!(promoted.severity(), Severity::Error);
        assert_eq!(promoted.code(), "INFERENCE_ON_VARIANT");
        assert_eq!(
            promoted.warning_code(),
            Some(WarningCode::InferenceOnVariant)
        );
    }
}
