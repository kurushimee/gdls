# differential: const initialised from another class's const (`HelperScript.ANSWER`). Hits the
# cross-file member-initializer resolution path (WP-R2). gdls resolves via the class_name registry
# + interface index; Godot resolves via global class registration. Both must emit zero errors.
extends RefCounted

const ECHOED: int = HelperScript.ANSWER

func is_correct() -> bool:
	return ECHOED == 42
