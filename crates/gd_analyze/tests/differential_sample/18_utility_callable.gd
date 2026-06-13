# differential: #88 — utility functions as first-class Callables (analyzer.cpp:4641-4652).
# Bare references to Variant utilities (print/floor/abs) and GDScript-only utilities
# (len/range) reduce to constant Callables; both sides must emit zero diagnostics.
extends Node

const PRINTER = print


func go() -> void:
	print.call_deferred("deferred")
	var f := floor
	var floored: Variant = f.call(1.5)
	var arr := [1.5, 2.5]
	var mapped: Array = arr.map(abs)
	var l := len
	var count: Variant = l.call("abc")
	var r := range
	var seq: Variant = r.call(3)
	PRINTER.call(floored, mapped, count, seq)
