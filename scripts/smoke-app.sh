#!/usr/bin/env bash
# Starts a built app with a throw-away data directory and checks that it comes
# up (window, tray, monitor thread) and keeps running, then stops it and
# everything it started. Meant for CI builds *without* the real-power
# feature: nothing can power off.
#
#   scripts/smoke-app.sh [path-to-binary]
set -euo pipefail

os=$(uname -s)
bin=${1:-target/release/agentgoodnight}
case "$os" in
  MINGW* | MSYS* | CYGWIN*) [[ "$bin" == *.exe ]] || bin="$bin.exe" ;;
esac
data=$(mktemp -d)
log="$data/app.log"
# WebView2 (Windows) gets its own profile as well.
export AGN_DATA_DIR="$data" WEBVIEW2_USER_DATA_FOLDER="$data/webview" NO_COLOR=1

if [[ "$os" == Linux ]]; then
  # Own process group, so Xvfb, the app and its WebKit processes stop
  # together; a D-Bus session like on a desktop.
  setsid dbus-run-session -- xvfb-run -a "$bin" >"$log" 2>&1 &
else
  "$bin" >"$log" 2>&1 &
fi
pid=$!

stop() {
  case "$os" in
    Linux) kill -- "-$pid" 2>/dev/null || true ;;
    MINGW* | MSYS* | CYGWIN*) taskkill //F //T //PID "$(cat "/proc/$pid/winpid")" >/dev/null 2>&1 || kill "$pid" 2>/dev/null || true ;;
    *) kill "$pid" 2>/dev/null || true ;;
  esac
}

start=$SECONDS
for _ in $(seq 1 60); do
  grep -q "main window opened" "$log" 2>/dev/null && break
  kill -0 "$pid" 2>/dev/null || break
  sleep 2
done
echo "main window after $((SECONDS - start)) s"
if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
  stages=$(grep -E "starting AgentGoodNight|setting up window|main window opened" "$log" | cut -c1-27,40-70 | tr '\n' ';')
  echo "::notice::$os: main window after $((SECONDS - start)) s ($stages)"
fi
# Keep running for a while with the window open.
sleep 15
status=0
if ! kill -0 "$pid" 2>/dev/null; then
  echo "::error::the app exited during start-up"
  status=1
fi
grep -q "starting AgentGoodNight" "$log" || { echo "::error::start-up line missing"; status=1; }
grep -q "DryRun" "$log" || { echo "::error::the build must use the DryRun power manager"; status=1; }
grep -q "main window opened" "$log" || { echo "::error::the main window was not created"; status=1; }
if grep -Eq "panicked| ERROR " "$log"; then
  echo "::error::errors in the app log"
  status=1
fi

if [[ "$status" != 0 && -n "${GITHUB_ACTIONS:-}" ]]; then
  # Readable without a login, unlike the job log.
  { echo "app log (tail):"; tail -n 40 "$log"; echo "processes:"; ps -eo pid,ppid,args 2>/dev/null | grep -iE "agentgoodnight|webkit|xvfb" | grep -v grep; } \
    | sed 's/%/%25/g' | awk 'BEGIN { ORS = "%0A" } { print }' | sed 's/^/::error::/'
  echo
fi

stop
for _ in $(seq 1 20); do
  kill -0 "$pid" 2>/dev/null || break
  sleep 0.5
done
if kill -0 "$pid" 2>/dev/null; then
  echo "::error::the app did not stop"
  status=1
fi
cat "$log"
exit "$status"
