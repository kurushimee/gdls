extends SceneTree
# regen-dump-omitted-methods.gd — re-derive gd_types' DUMP_OMITTED_NATIVE_METHODS table.
#
# #172: the `(class, method, is_virtual)` triples that Godot's ClassDB resolves but the shipped
# `extension_api.json` stock dump OMITS (Object::free + the per-class `_*` virtuals / internal
# MethodBinds). The table is hand-vendored in `crates/gd_types/src/native_db.rs` and can silently go
# stale when the vendored dump is bumped to a new Godot version. This tool re-derives it mechanically
# from a live Godot binary so the table is reproducible, not curated by hand.
#
# Run via the wrapper `scripts/regen-dump-omitted-methods.sh <godot-binary>` (it gunzips the vendored
# stock dump and feeds it here). Direct invocation:
#
#     godot --headless --script scripts/regen-dump-omitted-methods.gd -- <stock-dump.json> <out.txt>
#
# Derivation (mirrors the committed table's header doc): for every class the stock dump carries, take
# `ClassDB.class_get_method_list(cls, true)` (own methods only — the `no_inheritance` flag) and keep
# the names the dump's OWN method set for that class does not contain. `is_virtual` is the method's
# real `METHOD_FLAG_VIRTUAL` bit (`flags & 8`), NOT inferred from the `_`-prefix. Rows are emitted
# sorted by `(class, method)`, one per line, as paste-ready Rust:  `("Class", "method", <bool>),`.
#
# The output goes to the `<out.txt>` file (NOT stdout) so Godot's boot banner never contaminates it.
# Paste the lines between the `&[` / `];` markers of DUMP_OMITTED_NATIVE_METHODS, then `cargo fmt --all`
# (rustfmt re-wraps the few rows whose single-line form exceeds 100 columns).

const METHOD_FLAG_VIRTUAL := 8  # Godot's MethodFlags.METHOD_FLAG_VIRTUAL


func _init() -> void:
	var args := OS.get_cmdline_user_args()
	if args.size() < 2:
		push_error("usage: ... -- <stock-dump.json> <out.txt>")
		quit(2)
		return
	var dump_path: String = args[0]
	var out_path: String = args[1]

	var dump := _load_dump(dump_path)
	if dump.is_empty():
		quit(1)
		return

	# Guard: the dump must be for the exact version this binary exposes — diffing a 4.6.3 dump against
	# a 4.7 ClassDB (or vice-versa) would manufacture bogus rows. This is the drift the whole tool
	# exists to prevent, so refuse rather than emit a silently-wrong table.
	var v := Engine.get_version_info()
	var header: Dictionary = dump.get("header", {})
	var dump_ver := [
		int(header.get("version_major", -1)),
		int(header.get("version_minor", -1)),
		int(header.get("version_patch", -1)),
	]
	var bin_ver := [int(v.get("major", -2)), int(v.get("minor", -2)), int(v.get("patch", -2))]
	if dump_ver != bin_ver:
		push_error(
			"version mismatch: stock dump is %s but this Godot binary is %s — feed a matching dump"
			% [dump_ver, bin_ver]
		)
		quit(1)
		return

	var rows := _derive_rows(dump)
	rows.sort_custom(_row_less)

	var f := FileAccess.open(out_path, FileAccess.WRITE)
	if f == null:
		push_error("cannot open output file: %s" % out_path)
		quit(1)
		return
	for row in rows:
		f.store_line('    ("%s", "%s", %s),' % [row[0], row[1], "true" if row[2] else "false"])
	f.close()
	print("wrote %d rows to %s" % [rows.size(), out_path])
	quit(0)


# Per-class OWN method-name set from the stock dump, keyed by class name.
func _load_dump(path: String) -> Dictionary:
	if not FileAccess.file_exists(path):
		push_error("stock dump not found: %s" % path)
		return {}
	var text := FileAccess.get_file_as_string(path)
	var parsed: Variant = JSON.parse_string(text)
	if typeof(parsed) != TYPE_DICTIONARY:
		push_error("stock dump is not a JSON object: %s" % path)
		return {}
	return parsed


func _derive_rows(dump: Dictionary) -> Array:
	var rows: Array = []
	var classes: Array = dump.get("classes", [])
	for cls in classes:
		var class_id: String = cls.get("name", "")
		if class_id.is_empty() or not ClassDB.class_exists(class_id):
			# An editor/internal class the dump carries but ClassDB doesn't expose — nothing to diff.
			continue
		# The dump's OWN method names for this class (what gd_types already ingests).
		var dump_methods := {}
		for m in cls.get("methods", []):
			dump_methods[String(m.get("name", ""))] = true
		# ClassDB own-only methods — the superset gd_types is missing.
		for m in ClassDB.class_get_method_list(class_id, true):
			var name: String = m.get("name", "")
			if name.is_empty() or dump_methods.has(name):
				continue
			var is_virtual := (int(m.get("flags", 0)) & METHOD_FLAG_VIRTUAL) != 0
			rows.append([class_id, name, is_virtual])
	return rows


# Strict (class, method) ordering — matches the committed table's sort and Rust's tuple `<`.
func _row_less(a: Array, b: Array) -> bool:
	if a[0] != b[0]:
		return a[0] < b[0]
	return a[1] < b[1]
