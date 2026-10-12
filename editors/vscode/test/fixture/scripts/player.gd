class_name Player
extends Node

## How fast the player moves.
var speed: float = 10.0


func move(delta: float) -> float:
	return speed * delta
