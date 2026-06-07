# differential: typing warning under strict mode — `UNTYPED_DECLARATION` on a bare `var`.
# Fires only when `UNTYPED_DECLARATION` is promoted from its default `Ignore`; Godot's
# test-runner enables it, and gdls follows the strict-mode policy (docs/04). Both emit one
# warning per untyped declaration.
extends Node

func compute():
	var unannotated = 0
	unannotated += 1
	return unannotated
