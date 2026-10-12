# Changelog

## 1.0.1

- Server log lines show at the level gdls gave them, not all as errors.
- gdls starts with its log filter set to the output channel's log level, unless `GDLS_LOG` is already set.

## 1.0.0

The first release. The extension starts the `gdls` binary from `gdls.path` or `PATH` for every `.gd` file in a Godot project and passes through every gdls setting. It adds GDScript syntax highlighting and editing rules, a status bar item, and the **Restart Server** and **Show Output** commands.
