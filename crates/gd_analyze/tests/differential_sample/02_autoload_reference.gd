# differential: references the `GameState` autoload registered in project.godot. gdls resolves
# via the class_name registry / autoload table; Godot resolves via the engine's autoload singleton
# lookup. Both must emit zero diagnostics, treating `GameState.score` as a typed `int`.
extends Node

func bump() -> void:
	GameState.score += 1
