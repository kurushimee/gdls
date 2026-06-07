# differential: native-class member access (Node3D, Sprite3D). Resolves through the native DB on
# the gdls side and through ClassDB on Godot's side; both must emit zero diagnostics. Uses
# 3D variants because the analyzer conformance fixture (`trimmed_api.json`) includes Sprite3D
# but not Sprite2D — keeping the differential side's class resolution apples-to-apples with
# the conformance harness.
extends Node3D

var sprite: Sprite3D = null

func setup() -> void:
	sprite = Sprite3D.new()
	add_child(sprite)
	sprite.modulate = Color(1.0, 1.0, 1.0, 1.0)
