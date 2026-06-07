# differential: typed signal declaration, type-checked `emit`, and `connect` to a typed Callable.
# Exercises the analyzer's signal-arg arity/type matching and the connect-target resolution path.
extends Node

signal damage_taken(amount: int)

func on_damage(amount: int) -> void:
	print("ouch: ", amount)

func _ready() -> void:
	damage_taken.connect(on_damage)
	damage_taken.emit(7)
