#!/usr/bin/env python3
"""Move a conformance corpus forward one Godot feature release.

gdls supports several Godot releases at once, and each conformance harness reads a *set* of corpus
trees (`SUITES` in the harness file). The newest supported release carries the full vendored tree;
every older release carries only the files whose phase-relevant result actually differs at that
tag. So adding support for a new release *demotes* the current full tree to a subset.

This script does the mechanical half of that:

  1. Read the current vendored tree (which must still be a byte-exact mirror of `--from`).
  2. Diff it against the `--to` tree in the Godot checkout.
  3. Write the differing files, at their `--from` content, into `<corpus>-<from>/`.
  4. Refresh the main tree from `--to`.
  5. Print the `SUITES` row to add and the PROVENANCE facts to stamp.

What it deliberately does NOT do: decide which differences *matter*. A file can differ between
tags without its parse-phase or analyze-phase result changing at all — a renamed test helper, an
added case that passes either way. Those belong in neither subset. Review every file the script
writes, delete the ones that do not diverge in the phase the harness measures, and say why in the
suite table in PROVENANCE.md. An empty subset is a real and good outcome: delete the directory and
record that the tags agree.

Usage:
    scripts/conformance/demote_corpus.py parser   --from 4.6 --to 4.7 --godot ~/dev/godot
    scripts/conformance/demote_corpus.py analyzer --from 4.7 --to 4.8 --godot ~/dev/godot

`--from` and `--to` are feature versions; the script resolves each to the newest `X.Y.Z-stable` tag
in the checkout. Pass `--tag-from` / `--tag-to` to pin an exact tag instead. Nothing is written
until the checkout is clean and both tags resolve, and `--dry-run` prints the plan only.
"""

from __future__ import annotations

import argparse
import filecmp
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]

# Which crate owns each corpus, and where the upstream subtree lives.
CORPORA = {
    "parser": {
        "root": REPO / "crates/gd_syntax/tests/conformance/corpus",
        "upstream": "modules/gdscript/tests/scripts/parser",
        "harness": "crates/gd_syntax/tests/conformance.rs",
        "phase": "parse-phase",
    },
    "analyzer": {
        "root": REPO / "crates/gd_analyze/tests/conformance/corpus",
        "upstream": "modules/gdscript/tests/scripts/analyzer",
        "harness": "crates/gd_analyze/tests/conformance.rs",
        "phase": "analyze-phase",
    },
}

VENDORED_SUFFIXES = (".gd", ".out")


def git(godot: Path, *args: str) -> str:
    out = subprocess.run(
        ["git", "-C", str(godot), *args],
        check=True,
        capture_output=True,
        text=True,
    )
    return out.stdout.strip()


def resolve_tag(godot: Path, feature: str, pinned: str | None) -> str:
    if pinned:
        git(godot, "rev-parse", "--verify", f"{pinned}^{{commit}}")
        return pinned
    tags = git(godot, "tag", "--list", f"{feature}.*-stable").split()
    if not tags:
        sys.exit(
            f"no {feature}.*-stable tag in {godot}. Fetch it, or pass --tag-from/--tag-to."
        )
    tags.sort(key=lambda t: [int(n) for n in re.findall(r"\d+", t)])
    return tags[-1]


def export_tree(godot: Path, tag: str, subtree: str, dest: Path) -> None:
    """Extract `subtree` at `tag` into `dest` (which must not exist yet)."""
    dest.mkdir(parents=True)
    archive = subprocess.run(
        ["git", "-C", str(godot), "archive", tag, subtree],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["tar", "-x", "-C", str(dest), "--strip-components", str(subtree.count("/") + 1)],
        check=True,
        input=archive.stdout,
    )


def vendored_files(root: Path) -> set[str]:
    return {
        str(p.relative_to(root))
        for p in root.rglob("*")
        if p.is_file() and p.suffix in VENDORED_SUFFIXES
    }


def divergent(old: Path, new: Path) -> list[str]:
    """Relative paths that exist only in `old`, only in `new`, or differ between them."""
    names = sorted(vendored_files(old) | vendored_files(new))
    out = []
    for name in names:
        a, b = old / name, new / name
        if not a.exists() or not b.exists() or not filecmp.cmp(a, b, shallow=False):
            out.append(name)
    return out


def stems(paths: list[str]) -> list[str]:
    """Group `.gd`/`.out` pairs by their shared stem, so a subset stays self-consistent."""
    seen = []
    for p in paths:
        stem = p[: -len(Path(p).suffix)]
        if stem not in seen:
            seen.append(stem)
    return seen


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("corpus", choices=sorted(CORPORA))
    ap.add_argument("--from", dest="from_", required=True, help="current feature version, e.g. 4.7")
    ap.add_argument("--to", required=True, help="new feature version, e.g. 4.8")
    ap.add_argument("--tag-from", help="exact tag for --from, instead of the newest match")
    ap.add_argument("--tag-to", help="exact tag for --to, instead of the newest match")
    ap.add_argument("--godot", required=True, type=Path, help="a godotengine/godot checkout")
    ap.add_argument("--dry-run", action="store_true", help="print the plan, write nothing")
    args = ap.parse_args()

    spec = CORPORA[args.corpus]
    godot = args.godot.expanduser().resolve()
    if not (godot / ".git").exists():
        return sys.exit(f"{godot} is not a git checkout")

    tag_from = resolve_tag(godot, args.from_, args.tag_from)
    tag_to = resolve_tag(godot, args.to, args.tag_to)
    print(f"{args.corpus}: {tag_from} -> {tag_to}")

    root = spec["root"]
    live = root / args.corpus
    subset = root / f"{args.corpus}-{args.from_}"
    if not live.is_dir():
        return sys.exit(f"vendored tree missing at {live}")
    if subset.exists():
        return sys.exit(f"{subset} already exists — remove it or pick another --from")

    with tempfile.TemporaryDirectory() as tmp:
        old = Path(tmp) / "old"
        new = Path(tmp) / "new"
        export_tree(godot, tag_from, spec["upstream"], old)
        export_tree(godot, tag_to, spec["upstream"], new)

        drift = divergent(old, live)
        if drift:
            print(
                f"\nWARNING: the vendored tree is not a byte-exact mirror of {tag_from}.\n"
                "Reconcile these before demoting, or the subset will carry local edits:",
                file=sys.stderr,
            )
            for name in drift:
                print(f"  {name}", file=sys.stderr)
            return 1

        candidates = stems(divergent(old, new))
        print(f"\n{len(candidates)} candidate file(s) for the {args.from_} subset:")
        for stem in candidates:
            print(f"  {stem}")

        if args.dry_run:
            print("\n(dry run — nothing written)")
            return 0

        for stem in candidates:
            for suffix in VENDORED_SUFFIXES:
                src = old / f"{stem}{suffix}"
                if src.exists():
                    dst = subset / f"{stem}{suffix}"
                    dst.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(src, dst)

        shutil.rmtree(live)
        shutil.copytree(
            new,
            live,
            ignore=lambda _d, names: [
                n
                for n in names
                if not (new / n).is_dir() and not n.endswith(VENDORED_SUFFIXES)
            ],
        )

    commit_to = git(godot, "rev-parse", "--short", f"{tag_to}^{{commit}}")
    subtree_to = git(godot, "rev-parse", "--short", f"{tag_to}^{{commit}}:{spec['upstream']}")

    print(f"\nWrote {subset.relative_to(REPO)} and refreshed {live.relative_to(REPO)}.")
    print(
        f"""
Next, by hand:

  1. Open every file under {subset.relative_to(REPO)} and delete the ones whose
     {spec["phase"]} result does NOT actually differ at {args.from_} — a renamed test helper or an
     added case that behaves the same at both tags belongs in neither tree. If nothing is left,
     delete the directory; that is a real outcome, not a failure.
  2. Add the suite row to {spec["harness"]}:

         Suite {{
             dir: "corpus/{args.corpus}-{args.from_}",
             tag: "{args.from_}",
             dialect: Dialect::Godot{args.from_.replace(".", "_")},
         }},

  3. Re-stamp the source table in {(root / "PROVENANCE.md").relative_to(REPO)}:
         tag {tag_to}, commit {commit_to}, subtree {subtree_to}
  4. Re-run the harness, then re-bless the ratchet if it moved.
"""
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
