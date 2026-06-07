# differential: `const X = preload("res://_helper.gd")` — both gdls and Godot must resolve the
# sibling script via the project.godot-anchored res:// root and treat HELPER as a typed script ref.
extends Node

const HELPER = preload("res://_helper.gd")

func greet_world() -> String:
	return HELPER.new().greet("world")
