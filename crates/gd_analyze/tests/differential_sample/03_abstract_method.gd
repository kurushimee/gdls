# differential: `@abstract` function declaration. Both gdls and Godot accept the annotation
# on a function without a body and emit zero diagnostics on the declaring class.
@abstract
class_name AbstractBase
extends RefCounted

@abstract
func describe() -> String
