# differential: WP-F3 — unbalanced @warning_ignore_start / @warning_ignore_restore pair. Godot
# emits a parser-side diagnostic at parse time via GDScriptParser::Annotation::apply(); gdls
# today is silent (deferred from M3 WP-M; Phase E WP-F3 lands the diagnostic). This fixture
# will diverge until Phase E lands — kept here so the threshold tracks the gap closing.
extends Node

@warning_ignore_start("untyped_declaration")
var stays_ignored_forever = 0
