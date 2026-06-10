extends RefCounted

const SCALED = Vector3.ONE * 2.0


func go() -> Vector3:
	var v := Vector3.UP * 3.0
	var axis := Vector3.AXIS_X & 1
	return v * float(axis + int(SCALED.x))
