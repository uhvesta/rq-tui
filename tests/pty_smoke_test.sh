#!/usr/bin/env bash
set -euo pipefail

# --- begin runfiles.bash initialization v3 ---
set -uo pipefail
set +e
runfiles_library="bazel_tools/tools/bash/runfiles/runfiles.bash"
source "${RUNFILES_DIR:-/dev/null}/${runfiles_library}" 2>/dev/null || \
  source "$(grep -sm1 "^${runfiles_library} " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f2- -d' ')" 2>/dev/null || \
  source "$0.runfiles/${runfiles_library}" 2>/dev/null || \
  source "$(grep -sm1 "^${runfiles_library} " "$0.runfiles_manifest" | cut -f2- -d' ')" 2>/dev/null || \
  source "$(grep -sm1 "^${runfiles_library} " "$0.exe.runfiles_manifest" | cut -f2- -d' ')" 2>/dev/null || \
  { echo >&2 "ERROR: cannot find ${runfiles_library}"; exit 1; }
runfiles_library=
set -e
# --- end runfiles.bash initialization v3 ---

binary="$(rlocation "${TEST_WORKSPACE}/src/rq-tui")"
python_script="$(rlocation "${TEST_WORKSPACE}/tests/pty_smoke.py")"

[[ -x "${binary}" ]] || {
  echo "Bazel runfile is not executable: ${binary}" >&2
  exit 1
}
[[ -f "${python_script}" ]] || {
  echo "PTY helper runfile is missing: ${python_script}" >&2
  exit 1
}

exec python3 "${python_script}" "${binary}"
