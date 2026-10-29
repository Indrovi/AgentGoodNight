#!/usr/bin/env bash
# Starts a built app with a throw-away data directory and checks that it comes
# up (window, tray, monitor thread) and keeps running, then stops it. Meant
# for CI builds *without* the real-power feature: nothing can power off.
#
#   scripts/smoke-app.sh [path-to-binary]
set -euo pipefail

bin=${1:-target/release/agentgoodnight}
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) [[ "$bin" == *.exe ]] || bin="$bin.exe" ;;
esac
data=$(mktemp -d)
log="$data/app.log"
export AGN_DATA_DIR="$data" NO_COLOR=1

if [[ "$(uname -s)" == Linux ]]; then
  xvfb-run -a "$bin" >"$log" 2>&1 &
else
  "$bin" >"$log" 2>&1 &
fi
pid=$!

sleep 30
status=0
if ! kill -0 "$pid" 2>/dev/null; then
  echo "::error::the app exited during start-up"
  status=1
fi
grep -q "starting AgentGoodNight" "$log" || { echo "::error::start-up line missing"; status=1; }
grep -q "DryRun" "$log" || { echo "::error::the build must use the DryRun power manager"; status=1; }
if grep -Eq "panicked| ERROR " "$log"; then
  echo "::error::errors in the app log"
  status=1
fi
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
cat "$log"
exit "$status"
