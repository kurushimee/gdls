# differential: WP-F1 — duplicate `@icon` annotation. Godot emits parser-side
# "Annotation @icon can only be used once per script."; gdls today is silent (Phase E WP-F1
# lands the diagnostic and populates ClassNode::icon_path). Diverges until Phase E lands.
@icon("res://a.svg")
@icon("res://b.svg")
class_name DoubleIcon
extends RefCounted
