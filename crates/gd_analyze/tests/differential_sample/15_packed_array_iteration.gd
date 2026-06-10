extends RefCounted


func walk(paths: PackedStringArray, ids: PackedInt32Array) -> int:
	var total := 0
	for p in paths:
		total += p.length()
	for i in ids:
		total += i
	return total
