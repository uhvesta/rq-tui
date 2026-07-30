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
output_file="${TEST_TMPDIR}/non-tty-output.txt"

if "${binary}" review --pr example/repository#1 </dev/null >"${output_file}" 2>&1; then
  echo "non-interactive review unexpectedly succeeded" >&2
  exit 1
fi

grep -F "interactive review requires a TTY on stdin and stdout" "${output_file}" >/dev/null
if LC_ALL=C grep $'\033' "${output_file}" >/dev/null; then
  echo "non-interactive review emitted terminal escape sequences" >&2
  exit 1
fi
