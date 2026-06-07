# Helper for 06_preload_usage.gd / 07_const_external_member.gd: a tiny resource-friendly script
# with a typed const and a typed member function so the diff-target fixtures get a stable
# cross-file binding for both gdls and Godot to resolve.
class_name HelperScript
extends RefCounted

const ANSWER: int = 42

func greet(name: String) -> String:
	return "hello " + name
