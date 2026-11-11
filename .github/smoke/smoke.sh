#!/usr/bin/env bash
# canfit end-to-end smoke test: runs a release binary against synthetic inputs and checks exit
# codes, JSON output and the main commands. Works on Linux, macOS and Windows (Git Bash).
#
# usage: smoke.sh PATH/TO/canfit WORKDIR
#
# Everything canfit writes (history database, config, app profiles) goes under WORKDIR, through
# CANFIT_DATA_DIR / CANFIT_CONFIG / CANFIT_APPS. The only file written outside WORKDIR is a tiny
# settings file in $HOME/.canfit-smoke/, removed at the end. The test never closes anything:
# every command runs with stdin from /dev/null, so canfit never asks to close apps.
# Prints PASS/FAIL per check and exits 1 if any check failed.
set -u

BIN="$1"
WORK="$2"
# The script works inside WORKDIR: make a relative path to the binary absolute first.
if [ -e "$BIN" ]; then
  BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
fi
mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd)"
HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$WORK"

case "$(uname -s)" in
  Linux*) OS=linux ;;
  Darwin*) OS=macos ;;
  MINGW* | MSYS* | CYGWIN*) OS=windows ;;
  *) OS=other ;;
esac
PY=python3
command -v python3 > /dev/null 2>&1 && python3 -c 'import sys' > /dev/null 2>&1 || PY=python

export CANFIT_DATA_DIR="$WORK/data"
export CANFIT_CONFIG="$WORK/config.toml"
export CANFIT_APPS="$WORK/apps.toml"
unset NO_COLOR CANFIT_ASCII

pass=0
fail=0
failed=()

# Seconds one command may run before it counts as hung (SMOKE_TIMEOUT overrides).
SMOKE_TIMEOUT="${SMOKE_TIMEOUT:-120}"

# check NAME ALLOWED_EXIT_CODES(comma separated) COMMAND...
# A command still running after SMOKE_TIMEOUT seconds is killed and reported as a FAIL, so one
# hung command can't stall the whole run.
check() {
  local name="$1" want="$2"
  shift 2
  rm -f "$WORK/timed_out"
  "$@" > "$WORK/out.txt" 2> "$WORK/err.txt" < /dev/null &
  local pid=$!
  (
    i=0
    while kill -0 "$pid" 2> /dev/null; do
      sleep 1
      i=$((i + 1))
      if [ "$i" -ge "$SMOKE_TIMEOUT" ]; then
        : > "$WORK/timed_out"
        kill -9 "$pid" 2> /dev/null
        # On Windows the hung canfit.exe may be a grandchild (env, shell functions): kill it too.
        [ "$OS" = windows ] && taskkill //F //T //IM canfit.exe > /dev/null 2>&1
        break
      fi
    done
  ) > /dev/null 2>&1 &
  local watchdog=$!
  wait "$pid"
  local rc=$?
  wait "$watchdog" 2> /dev/null
  if [ -e "$WORK/timed_out" ]; then
    echo "FAIL [hung: killed after ${SMOKE_TIMEOUT}s, want $want] $name"
    echo "  stdout: $(head -c 600 "$WORK/out.txt")"
    echo "  stderr: $(head -c 600 "$WORK/err.txt")"
    fail=$((fail + 1))
    failed+=("$name (hung)")
    return
  fi
  if [[ ",$want," == *",$rc,"* ]]; then
    echo "PASS [$rc] $name"
    pass=$((pass + 1))
  else
    echo "FAIL [$rc, want $want] $name"
    echo "  stdout: $(head -c 600 "$WORK/out.txt")"
    echo "  stderr: $(head -c 600 "$WORK/err.txt")"
    fail=$((fail + 1))
    failed+=("$name")
  fi
}

# assert NAME SHELL_CONDITION (evaluated after the last check)
assert() {
  if eval "$2"; then
    echo "PASS      $1"
    pass=$((pass + 1))
  else
    echo "FAIL      $1"
    echo "  stdout: $(head -c 600 "$WORK/out.txt")"
    fail=$((fail + 1))
    failed+=("$1")
  fi
}

json() { "$PY" -c 'import json,sys; d=json.load(open(sys.argv[1], encoding="utf-8")); print(eval("d" + sys.argv[2]))' "$WORK/out.txt" "$1" 2> /dev/null; }
json_ok() { "$PY" -c 'import json,sys; json.load(open(sys.argv[1], encoding="utf-8"))' "$WORK/out.txt" 2> /dev/null; }
no_ansi() { ! grep -q "$(printf '\033')\[" "$WORK/out.txt"; }

# A broken fixture would make later checks fail for the wrong reason: stop here instead.
"$PY" "$HERE/mkgguf.py" tiny tiny.gguf || { echo "FATAL: could not write tiny.gguf"; exit 1; }
"$PY" "$HERE/mkgguf.py" huge huge.gguf || { echo "FATAL: could not write huge.gguf"; exit 1; }
printf 'not a model at all' > bogus.gguf
head -c 200 tiny.gguf > truncated.gguf
mkdir -p models empty
cp tiny.gguf models/small.gguf

echo "== canfit smoke test on $OS ($(uname -m)): $("$BIN" --version)"

# --- basics and usage errors ----------------------------------------------------------------
check "--version" 0 "$BIN" --version
assert "version line starts with canfit" 'grep -q "^canfit [0-9]" out.txt'
# The version of the checkout this script belongs to (1.0.0 for the first release).
WANT_VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$HERE/../../Cargo.toml" | head -n 1)"
assert "--version says canfit ${WANT_VERSION:-?}" '[ -n "$WANT_VERSION" ] && [ "$(tr -d "\r" < out.txt)" = "canfit $WANT_VERSION" ]'
check "--help" 0 "$BIN" --help
assert "help lists the exit codes" 'grep -q "Exit codes" out.txt'
for sub in free watch history models update; do
  check "$sub --help" 0 "$BIN" "$sub" --help
done
check "unknown flag -> 64" 64 "$BIN" --no-such-flag
check "--ctx 0 -> 64" 64 "$BIN" tiny.gguf --ctx 0
check "--cache-type f32 -> 64" 64 "$BIN" tiny.gguf --cache-type f32
check "--margin 500 -> 64" 64 "$BIN" tiny.gguf --margin 500
check "--interval 0.1 -> 64" 64 "$BIN" --interval 0.1

# --- snapshot (stdout is not a terminal: one-shot) --------------------------------------------
check "snapshot" 0 "$BIN"
assert "snapshot is not empty" '[ -s out.txt ]'
assert "piped output has no colour codes" no_ansi
check "snapshot --json" 0 "$BIN" --json
assert "snapshot JSON schema" '[ "$(json "[\"schema\"]")" = canfit.snapshot ]'
assert "snapshot JSON platform" '[ "$(json "[\"platform\"][\"os\"]")" = "$OS" ]'
assert "snapshot total memory > 0" '[ "$(json "[\"memory\"][\"total_bytes\"]")" -gt 0 ]'
check "NO_COLOR snapshot" 0 env NO_COLOR=1 "$BIN"
assert "NO_COLOR output has no colour codes" no_ansi
check "CANFIT_ASCII=1 snapshot" 0 env CANFIT_ASCII=1 "$BIN"
assert "ASCII output has no box drawing" '! grep -q "╭" out.txt'
for theme in dark light high-contrast; do
  check "--theme $theme" 0 "$BIN" --theme "$theme"
done

# --- model fit check ----------------------------------------------------------------------------
check "tiny model fits" 0,1 "$BIN" tiny.gguf
check "tiny model --json" 0,1 "$BIN" tiny.gguf --json --ctx 2048 --cache-type q8_0
assert "fit JSON schema" '[ "$(json "[\"schema\"]")" = canfit.fit ]'
assert "fit JSON verdict is fits/tight" '[[ "$(json "[\"verdict\"]")" =~ ^(fits|tight)$ ]]'
assert "fit JSON weights are exact" '[ "$(json "[\"needed\"][\"weights_bytes\"]")" -gt 0 ]'
check "huge model doesn't fit -> 2" 2 "$BIN" huge.gguf
check "huge model --json -> 2" 2 "$BIN" huge.gguf --json
assert "huge JSON verdict doesnt_fit" '[ "$(json "[\"verdict\"]")" = doesnt_fit ]'
assert "huge JSON exit_code is 2" '[ "$(json "[\"exit_code\"]")" = 2 ]'
check "huge --make-room never prompts when piped" 2 "$BIN" huge.gguf --make-room
check "free huge.gguf" 0,1,2 "$BIN" free huge.gguf
check "two models together" 0,1,2 "$BIN" tiny.gguf tiny.gguf --json
assert "combined JSON parses" json_ok
check "missing file -> 3" 3 "$BIN" missing.gguf
check "not a GGUF -> 3" 3 "$BIN" bogus.gguf
assert "explains the bad file" 'grep -qi "not a GGUF" err.txt'
check "truncated GGUF -> 3" 3 "$BIN" truncated.gguf
check "unknown app -> 3" 3 "$BIN" "No Such App 42"

# --- models -----------------------------------------------------------------------------------------
check "models DIR" 0 "$BIN" models models
assert "models lists the file" 'grep -q "small" out.txt'
check "models DIR --json" 0 "$BIN" models models --json
assert "models JSON parses" json_ok
check "folder as target" 0,1,2 "$BIN" models
check "models in an empty folder" 0,3 "$BIN" models empty

# --- free ---------------------------------------------------------------------------------------------
check "free" 0 "$BIN" free
check "free --json" 0 "$BIN" free --json
assert "free JSON parses" json_ok

# --- apps -----------------------------------------------------------------------------------------------
mkdir -p "$HOME/.canfit-smoke"
cat > "$CANFIT_APPS" << 'EOF'
schema_version = 1

[[app]]
name = "Smoke Test VM"
aliases = ["smokevm"]
readers = [
  { kind = "json", key = "vm.memoryMiB", unit = "MiB", label = "Smoke VM",
    paths = { macos = ["{home}/.canfit-smoke/vm.json"], windows = ["{home}/.canfit-smoke/vm.json"], linux = ["{home}/.canfit-smoke/vm.json"] } },
]
EOF
printf '{"vm": {"memoryMiB": 512}}' > "$HOME/.canfit-smoke/vm.json"
check "app from declared config (512 MiB) fits" 0,1 "$BIN" "Smoke Test VM"
check "app alias, --json" 0,1 "$BIN" smokevm --json
assert "app JSON parses" json_ok
printf '{"vm": {"memoryMiB": 104857600}}' > "$HOME/.canfit-smoke/vm.json"
check "app from declared config (100 TiB) -> 2" 2 "$BIN" "Smoke Test VM"
check "app + model together -> 2" 2 "$BIN" "Smoke Test VM" tiny.gguf
printf 'not json' > "$HOME/.canfit-smoke/vm.json"
check "unreadable app settings -> 3" 3 "$BIN" "Smoke Test VM"
printf 'this is = = not toml' > "$CANFIT_APPS"
check "broken apps.toml -> 3" 3 "$BIN" "Smoke Test VM"
rm -f "$CANFIT_APPS"
rm -rf "$HOME/.canfit-smoke"
check "catalog app (Slack)" 0,1,2 "$BIN" Slack
check "--app history" 0,1,2,3 "$BIN" --app history

# --- watch ----------------------------------------------------------------------------------------------
check "watch -- true" 0 "$BIN" watch -- true
check "watch -- false -> 1" 1 "$BIN" watch -- false
check "watch -- exit 7 -> 7" 7 "$BIN" watch -- sh -c 'exit 7'
check "watch -- missing command -> 127" 127 "$BIN" watch -- definitely-not-a-command-xyz
check "watch passes stdout through" 0 "$BIN" watch -- echo hello-from-child
assert "child stdout untouched" '[ "$(tr -d "\r" < out.txt)" = hello-from-child ]'
assert "canfit lines only on stderr" '! grep -q "\[canfit\]" out.txt'
check "watch a 300 MB allocation" 0 "$BIN" watch -- "$PY" -c 'import time; b = bytearray(300 * 1024 * 1024); time.sleep(2)'
assert "watch prints a [canfit] summary on stderr" 'grep -q "\[canfit\]" err.txt'
if [ "$OS" != windows ]; then
  check "watch -- killed by SIGTERM -> 143" 143 "$BIN" watch -- sh -c 'kill -TERM $$'
fi

# --- history ----------------------------------------------------------------------------------------------
check "history" 0 "$BIN" history
check "history --json" 0 "$BIN" history --json
assert "history JSON parses" json_ok
check "history --apps" 0 "$BIN" history --apps
check "history --apps --json" 0 "$BIN" history --apps --json
assert "history remembers the watched python run" 'grep -qi "python" out.txt'
assert "history database is inside CANFIT_DATA_DIR" '[ -n "$(ls "$CANFIT_DATA_DIR" 2> /dev/null)" ]'

# --- history opt-out: --no-history, CANFIT_NO_HISTORY=1, watch --no-save ---------------------------
# Each runs against an empty data directory that must stay empty, with the usual exit codes.
OFF="$WORK/data-off"
rm -rf "$OFF"
mkdir -p "$OFF"
off() { env CANFIT_DATA_DIR="$OFF" "$@"; }
off_empty() { [ -z "$(ls -A "$OFF" 2> /dev/null)" ]; }
last_err() { tail -n 1 "$WORK/err.txt" | tr -d '\r'; }
ALLOC=("$PY" -c 'import time; b = bytearray(50 * 1024 * 1024); time.sleep(1.5)')
check "--no-history snapshot" 0 off "$BIN" --no-history
check "--no-history snapshot --json" 0 off "$BIN" --json --no-history
check "--no-history tiny model" 0,1 off "$BIN" --no-history tiny.gguf
check "--no-history huge model -> 2" 2 off "$BIN" huge.gguf --no-history
check "--no-history unknown app -> 3" 3 off "$BIN" --no-history "No Such App 42"
check "--no-history Slack (catalog)" 0,1,2 off "$BIN" --no-history Slack
check "--no-history free" 0 off "$BIN" --no-history free
check "--no-history history --json" 0 off "$BIN" --no-history history --json
assert "history JSON says history off" '[ "$(json "[\"history\"]")" = off ]'
check "--no-history history --apps" 0 off "$BIN" history --apps --no-history
check "--no-history watch -- exit 7 -> 7" 7 off "$BIN" --no-history watch -- sh -c 'exit 7'
check "--no-history watch (50 MB)" 0 off "$BIN" --no-history watch -- "${ALLOC[@]}"
assert "summary ends with Not saved (--no-history)." '[ "$(last_err)" = "Not saved (--no-history)." ]'
check "watch --no-save (50 MB)" 0 off "$BIN" watch --no-save --json-out "$WORK/nosave.json" -- "${ALLOC[@]}"
assert "summary ends with Not saved (--no-save)." '[ "$(last_err)" = "Not saved (--no-save)." ]'
assert "--json-out says history off" 'grep -q "\"history\": \"off\"" "$WORK/nosave.json"'
check "watch --no-save -- false -> 1" 1 off "$BIN" watch --no-save -- false
check "watch --no-save -- missing command -> 127" 127 off "$BIN" watch --no-save -- definitely-not-a-command-xyz
check "CANFIT_NO_HISTORY=1 watch (50 MB)" 0 off env CANFIT_NO_HISTORY=1 "$BIN" watch -- "${ALLOC[@]}"
assert "summary ends with Not saved (CANFIT_NO_HISTORY=1)." '[ "$(last_err)" = "Not saved (CANFIT_NO_HISTORY=1)." ]'
check "CANFIT_NO_HISTORY=1 tiny model" 0,1 off env CANFIT_NO_HISTORY=1 "$BIN" tiny.gguf
check "CANFIT_NO_HISTORY=1 history --apps --json" 0 off env CANFIT_NO_HISTORY=1 "$BIN" history --apps --json
assert "learned-apps JSON says history off" '[ "$(json "[\"history\"]")" = off ]'
assert "the opt-out data directory is still empty" off_empty
# With an existing database, an opt-out run leaves it byte for byte unchanged.
cksum "$CANFIT_DATA_DIR/history.sqlite3" > "$WORK/db-before.txt" 2> /dev/null
check "watch --no-save with an existing history" 0 "$BIN" watch --no-save -- "${ALLOC[@]}"
check "--no-history Slack with an existing history" 0,1,2 "$BIN" --no-history Slack
assert "existing history database unchanged" '[ -s "$WORK/db-before.txt" ] && cksum "$CANFIT_DATA_DIR/history.sqlite3" | cmp -s - "$WORK/db-before.txt"'

# --- update (says how; never updates when piped) -------------------------------------------------------------
check "update" 0,1,3 "$BIN" update

echo "---- $OS: $pass passed, $fail failed"
if [ "$fail" -gt 0 ]; then
  printf '  failed: %s\n' "${failed[@]}"
  exit 1
fi
