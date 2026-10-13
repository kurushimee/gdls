#!/usr/bin/env bash
# The whole CI gate, run on this machine: every job `.github/workflows/ci.yml` and `vscode.yml` run on a pull request, so a PR can merge on a local green without waiting for CI.
#
#   scripts/gate.sh            run the gate on the checkout it is called from (the working tree, committed or not)
#   scripts/gate.sh --clean    delete the Windows mirror, its temp folder, and the Xvfb symlink, then exit
#
# Legs, all reported in the SUMMARY block at the end (read that block, not the streamed log):
#   fmt, lint, fuzz-check, test   the Linux leg, as on ubuntu-latest
#   windows                       `cargo test --workspace` on Windows, from WSL through cmd.exe (only under WSL)
#   vscode                        typecheck, test in a real VS Code under Xvfb, package; runs when editors/vscode or vscode.yml changed against origin/main, or with GATE_VSCODE=1
#   release                       the LTO release build, only with GATE_RELEASE=1 (the vscode leg builds it anyway)
#
# Machine settings come from the environment or from ~/.config/gdls-gate.env (see scripts/CLAUDE.md):
#   GDLS_WIN_TREE   Windows path of the disposable Windows mirror, e.g. F:\gdls-wintest
#   GDLS_SYSROOT    folder holding the unpacked Xvfb and VS Code's missing libraries, when the system has none
set -uo pipefail

[ -f "$HOME/.config/gdls-gate.env" ] && . "$HOME/.config/gdls-gate.env"

top=$(git rev-parse --show-toplevel 2>/dev/null)
[ -f "$top/crates/gd_server/Cargo.toml" ] || { echo "run this from inside a gdls checkout" >&2; exit 2; }
cd "$top"

on_wsl=0
command -v cmd.exe >/dev/null 2>&1 && command -v wslpath >/dev/null 2>&1 && on_wsl=1
win_tree=${GDLS_WIN_TREE:-}
sysroot=${GDLS_SYSROOT:-}
# Xvfb finds xkbcomp through a path compiled into it. The sysroot copy is patched to look in /tmp/xkb, a path as long as the original /usr/bin, and this symlink points it at the sysroot.
xkb_link=/tmp/xkb

if [ "${1:-}" = "--clean" ]; then
    if [ -n "$win_tree" ] && [ $on_wsl = 1 ]; then
        wsl_tree=$(wslpath -u "$win_tree")
        # A test process left running keeps its files locked, so stop anything started from the mirror first.
        powershell.exe -NoProfile -Command "Get-Process | Where-Object { \$_.Path -like '$win_tree\\*' } | Stop-Process -Force" 2>/dev/null
        rm -rf "$wsl_tree" "$wsl_tree-tmp"
        echo "removed $win_tree and $win_tree-tmp"
    fi
    rm -f "$xkb_link"
    exit 0
fi

logs=$(mktemp -d /tmp/gdls-gate.XXXXXX)
echo "gating $(pwd) ($(git branch --show-current 2>/dev/null)), logs in $logs"
declare -A verdict

# ---------------------------------------------------------------- windows (started first, runs in the background)
win_pid=
win_rc_file=$logs/windows.rc
if [ $on_wsl = 1 ]; then
    if [ -z "$win_tree" ]; then
        verdict[windows]="FAILED (GDLS_WIN_TREE is not set; see scripts/CLAUDE.md)"
    else
        (
            wsl_tree=$(wslpath -u "$win_tree")
            wsl_tmp=$wsl_tree-tmp
            mkdir -p "$wsl_tree"
            # The mirror is a plain copy of the working tree with no git of its own, so it tests exactly what is on disk here, committed or not. Ignored files are neither copied nor deleted, which keeps the mirror's target/ warm between runs.
            rsync -a --delete --exclude=/.git/ --filter=':- .gitignore' ./ "$wsl_tree/" || { echo 1 > "$win_rc_file"; exit; }
            # CI points TMP at the runner's work drive; the tests create and delete thousands of temp projects. The folder is the gate's own, so it is wiped before and after every run.
            rm -rf "$wsl_tmp" && mkdir -p "$wsl_tmp"
            cd "$wsl_tree" || exit
            # WSLENV hands these to the Windows side; /p translates the temp path. The values match ci.yml's env block.
            TMP=$wsl_tmp TEMP=$wsl_tmp RUSTFLAGS="-D warnings" CARGO_PROFILE_DEV_DEBUG=0 CARGO_TERM_COLOR=never \
                WSLENV=TMP/p:TEMP/p:RUSTFLAGS:CARGO_PROFILE_DEV_DEBUG:CARGO_TERM_COLOR \
                cmd.exe /c "cargo test --workspace" > "$logs/windows.log" 2>&1 < /dev/null
            echo $? > "$win_rc_file"
            powershell.exe -NoProfile -Command "Get-Process | Where-Object { \$_.Path -like '$win_tree\\*' } | Stop-Process -Force" > /dev/null 2>&1
            rm -rf "$wsl_tmp"
        ) &
        win_pid=$!
    fi
fi

# ---------------------------------------------------------------- linux
echo "=== fmt ==="
cargo fmt --all --check > "$logs/fmt.log" 2>&1; rc=$?
[ $rc -eq 0 ] && verdict[fmt]=ok || { verdict[fmt]=FAILED; head -40 "$logs/fmt.log"; }

echo "=== lint ==="
cargo lint > "$logs/lint.log" 2>&1; rc=$?
[ $rc -eq 0 ] && verdict[lint]=ok || { verdict[lint]=FAILED; grep -E "^(error|warning)" -A8 "$logs/lint.log" | head -40; }

echo "=== fuzz-check ==="
cargo check --manifest-path fuzz/Cargo.toml --bins --target-dir target > "$logs/fuzz-check.log" 2>&1; rc=$?
[ $rc -eq 0 ] && verdict[fuzz-check]=ok || { verdict[fuzz-check]=FAILED; grep -E "^error" -A8 "$logs/fuzz-check.log" | head -40; }

test_counts() { grep -E "^test result:" "$1" | awk -F'[ ;]' '{p+=$4; f+=$6} END{print p" passed, "f" failed"}'; }

echo "=== test ==="
cargo test --workspace > "$logs/test.log" 2>&1; rc=$?
[ $rc -eq 0 ] && verdict[test]="ok — $(test_counts "$logs/test.log")" || { verdict[test]="FAILED — $(test_counts "$logs/test.log")"; grep -E "^test result: FAILED|^failures:|panicked at|^error" -A6 "$logs/test.log" | head -60; }
grep -iE "fidelity: [0-9]+/[0-9]+" "$logs/test.log" | head -4

# ---------------------------------------------------------------- release + vscode
run_vscode=${GATE_VSCODE:-0}
if [ "$run_vscode" = 0 ]; then
    base=$(git merge-base HEAD origin/main 2>/dev/null)
    if [ -n "$base" ] && { git diff --name-only "$base"; git ls-files --others --exclude-standard; } | grep -qE "^(editors/vscode/|\.github/workflows/vscode\.yml)"; then
        run_vscode=1
    fi
fi

if [ "${GATE_RELEASE:-0}" = 1 ] || [ "$run_vscode" = 1 ]; then
    echo "=== release build ==="
    cargo build --release --bin gdls > "$logs/release.log" 2>&1; rc=$?
    [ $rc -eq 0 ] && verdict[release]=ok || { verdict[release]=FAILED; tail -10 "$logs/release.log"; }
fi

if [ "$run_vscode" = 1 ]; then
    echo "=== vscode ==="
    xvfb_pid=
    vscode_rc=0
    (
        cd editors/vscode || exit 1
        npm ci && npm run typecheck || exit 1
        export PATH="$top/target/release:$PATH" WAYLAND_DISPLAY=
        if command -v xvfb-run >/dev/null 2>&1; then
            xvfb-run -a npm test || exit 1
        else
            exit 3
        fi
        npx vsce package --out "$logs/gdls.vsix"
    ) > "$logs/vscode.log" 2>&1; vscode_rc=$?
    if [ $vscode_rc -eq 3 ] && [ -n "$sysroot" ] && [ -x "$sysroot/usr/bin/Xvfb" ]; then
        # No system xvfb-run: start the sysroot's Xvfb on a free display and point VS Code at it.
        ln -sfn "$sysroot/usr/bin" "$xkb_link"
        display=99
        while [ -e "/tmp/.X11-unix/X$display" ] || [ -e "/tmp/.X$display-lock" ]; do display=$((display + 1)); done
        LD_LIBRARY_PATH="$sysroot/usr/lib/x86_64-linux-gnu" "$sysroot/usr/bin/Xvfb" ":$display" -screen 0 1280x1024x24 -nolisten tcp > "$logs/xvfb.log" 2>&1 &
        xvfb_pid=$!
        sleep 1
        (
            cd editors/vscode || exit 1
            export PATH="$top/target/release:$PATH" WAYLAND_DISPLAY= DISPLAY=":$display" LD_LIBRARY_PATH="$sysroot/usr/lib/x86_64-linux-gnu"
            npm test && npx vsce package --out "$logs/gdls.vsix"
        ) >> "$logs/vscode.log" 2>&1; vscode_rc=$?
        kill "$xvfb_pid" 2>/dev/null
    fi
    if [ $vscode_rc -eq 0 ]; then
        verdict[vscode]="ok — $(grep -cE '^  ok ' "$logs/vscode.log") cases"
    elif [ $vscode_rc -eq 3 ]; then
        verdict[vscode]="FAILED (no xvfb-run and no GDLS_SYSROOT Xvfb; see scripts/CLAUDE.md)"
    else
        verdict[vscode]=FAILED
        grep -E "FAIL|Error|error" "$logs/vscode.log" | head -30
    fi
fi

# ---------------------------------------------------------------- collect windows
if [ -n "$win_pid" ]; then
    echo "=== windows (waiting) ==="
    wait "$win_pid"
    rc=$(cat "$win_rc_file" 2>/dev/null || echo 1)
    if [ "$rc" = 0 ]; then
        verdict[windows]="ok — $(test_counts "$logs/windows.log")"
    else
        verdict[windows]="FAILED — $(test_counts "$logs/windows.log")"
        grep -E "^test result: FAILED|^failures:|panicked at|^error" -A6 "$logs/windows.log" | head -60
    fi
fi

echo
echo "=== SUMMARY ==="
fail=0
for leg in fmt lint fuzz-check test windows release vscode; do
    v=${verdict[$leg]:-}
    if [ -z "$v" ]; then
        case $leg in
            windows) v="skipped (not under WSL)" ;;
            release) v="skipped (GATE_RELEASE=1 builds it)" ;;
            vscode) v="skipped (no extension change; GATE_VSCODE=1 runs it)" ;;
        esac
    fi
    case $v in FAILED*) fail=1 ;; esac
    printf '%-11s %s\n' "$leg" "$v"
done
echo "logs       $logs"
[ $fail -eq 0 ] && echo "GATE-COMPLETE ok" || echo "GATE-COMPLETE FAILED"
exit $fail
