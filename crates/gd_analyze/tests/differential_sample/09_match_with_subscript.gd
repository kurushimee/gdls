# differential: `match` over a subscripted expression. Hits Godot's null-source line override
# path inside the match-pattern reducer (the analyzer must synthesize a non-null line for the
# emitted diagnostic ranges). gdls mirrors via Diagnostic::line_override (WP-R3).
extends Node

func classify(values: Array) -> String:
	match values[0]:
		0:
			return "zero"
		1, 2, 3:
			return "small"
		_:
			return "other"
