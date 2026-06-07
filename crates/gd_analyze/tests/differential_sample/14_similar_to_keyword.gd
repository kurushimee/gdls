# differential: WP-F4 — identifier visually similar to a GDScript keyword. The Cyrillic 'с'
# (U+0441) renders identically to ASCII 'c' but is a different codepoint, so `сlass_name` is a
# legal identifier that looks like the keyword `class_name`. Godot emits
# IDENTIFIER_SIMILAR_TO_KEYWORD via TextServer::is_confusable; gdls today is silent (Phase E WP-F4
# lands the unicode-security skeleton check). Diverges until Phase E lands.
extends Node

var сlass_name: int = 0

func _ready() -> void:
	сlass_name = 1
