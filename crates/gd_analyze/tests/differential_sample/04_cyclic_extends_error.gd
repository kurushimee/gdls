# differential: cyclic inheritance — a class extending itself trips resolve_inheritance's cycle
# detector. Both gdls and Godot must emit an error; the wording differs across builds, so the
# differential harness compares diagnostic-code sets (both produce one error code).
class_name SelfCycle
extends SelfCycle
