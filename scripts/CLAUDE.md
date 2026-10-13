# scripts

## `gate.sh`, the whole CI gate on one machine

`scripts/gate.sh` runs every job CI runs on a pull request: fmt, lint, the fuzz-crate compile check, and the tests on Linux, the tests on Windows, and the VS Code extension's typecheck, test, and package. A PR merges once the gate's `SUMMARY` block reads `GATE-COMPLETE ok`. CI is only checked once, on `main` after the merge (`gh run list --branch main --limit 5`, then `gh run view <id> --json jobs` for per-job verdicts), because it is slower than the local run.

The gate tests the working tree as it is on disk, committed or not. Logs for every leg go to a fresh `/tmp/gdls-gate.*` folder named in the summary. The vscode leg runs only when `editors/vscode/` or `vscode.yml` changed against `origin/main` (the same path filter CI uses), or with `GATE_VSCODE=1`. The nightly libFuzzer run is not part of the gate; run a target by hand with `cargo +nightly fuzz run <target>` when a change touches what it fuzzes.

Machine settings live in `~/.config/gdls-gate.env`, which the gate sources:

```bash
GDLS_WIN_TREE='F:\gdls-wintest'
GDLS_SYSROOT=/home/kuru/dev/tools/sysroot
```

### Windows leg

Under WSL the gate runs `cargo test --workspace` with the Windows toolchain through `cmd.exe`, in the background while the Linux legs run. It needs `cargo` on the Windows side (`rustup` installs the toolchain `rust-toolchain.toml` pins on first build).

- `GDLS_WIN_TREE` is a disposable mirror, refreshed by `rsync` from the working tree on every run. It has no `.git` and is never edited by hand, so there is no such thing as "uncommitted changes" in it, and its state is never a reason to stop or ask. Files ignored by `.gitignore` are neither copied nor deleted, so its `target/` stays warm between runs.
- The tests' temp projects go to `<tree>-tmp` (CI does the same with the runner's work drive). The gate wipes that folder before and after each run and stops any process still running from the mirror's path, so a hung or crashed test leaves nothing behind. It never kills by image name, since a `gdls.exe` from an editor may be running.
- `scripts/gate.sh --clean` removes the mirror, the temp folder, and the Xvfb symlink. The next run rebuilds the mirror from scratch, which costs one cold Windows build (several minutes).

### vscode leg

`npm test` starts a real VS Code, which needs an X display and `libnss3`, `libnspr4`, and `libasound2`. With a system `xvfb-run` the gate uses that. Without one (no sudo on this box), `GDLS_SYSROOT` holds the packages unpacked by hand, and the gate starts its `Xvfb` on a free display:

```bash
mkdir -p ~/dev/tools/sysroot/debs && cd ~/dev/tools/sysroot/debs
apt-get download libasound2-data libasound2t64 libfontenc1 libnspr4 libnss3 libxfont2 x11-xkb-utils xserver-common xvfb
for d in *.deb; do dpkg-deb -x "$d" ~/dev/tools/sysroot; done
python3 -c "p='$HOME/dev/tools/sysroot/usr/bin/Xvfb'; d=open(p,'rb').read(); assert d.count(b'/usr/bin\x00')==1; open(p,'wb').write(d.replace(b'/usr/bin\x00', b'/tmp/xkb\x00'))"
```

The last line is needed because `Xvfb` runs `xkbcomp` from a directory compiled into it (`/usr/bin`), and there is no flag to change it. The patch swaps in `/tmp/xkb`, which has the same length, and the gate points that symlink at the sysroot's `usr/bin` before each start. Without it `Xvfb` dies with "Failed to activate virtual core keyboard". The list of packages is what `apt-cache depends --recurse` reports as missing on this box; on another machine, recompute it.
