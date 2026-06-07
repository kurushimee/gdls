# differential: typed lambda with an explicit return type plus an enclosing-scope capture.
# Exercises the reducer's lambda-typing path and the analyzer's capture-binding resolution.
extends Node

func make_adder(base: int) -> Callable:
	var add := func(x: int) -> int:
		return base + x
	return add
