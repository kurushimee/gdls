# differential: trivial well-typed script — both gdls and Godot must emit zero diagnostics.
extends Node

var counter: int = 0

func tick() -> void:
	counter += 1
