extends Node


func _ready() -> void:
	var player := Player.new()
	var moved: int = "not a number"
	player.move(1.0)
	print(moved)
