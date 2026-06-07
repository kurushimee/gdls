# differential: WP-F2 — duplicate `@tool` annotation. Godot emits parser-side
# "Annotation @tool can only be used once per script."; gdls today is silent (Phase E WP-F2
# lands the diagnostic). Diverges until Phase E lands.
@tool
@tool
extends Node
