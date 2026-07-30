#!/usr/bin/env bash
set -euo pipefail

source_script="${TEST_SRCDIR}/${TEST_WORKSPACE}/bootstrap-bazelisk.sh"
case_root="${TEST_TMPDIR}/bootstrap-guards"

run_rejected_case() {
  local name="$1"
  local expected="$2"
  local case_dir="${case_root}/${name}"
  local output="${case_dir}/output"

  mkdir -p "${case_dir}"
  cp "${source_script}" "${case_dir}/bootstrap-bazelisk.sh"
  chmod 0755 "${case_dir}/bootstrap-bazelisk.sh"
  shift 2
  "$@" "${case_dir}"

  if "${case_dir}/bootstrap-bazelisk.sh" >"${output}" 2>&1; then
    echo "Expected bootstrap case '${name}' to fail." >&2
    exit 1
  fi
  if ! grep -F "${expected}" "${output}" >/dev/null; then
    echo "Bootstrap case '${name}' did not report '${expected}'." >&2
    cat "${output}" >&2
    exit 1
  fi
}

make_symlinked_tools() {
  local case_dir="$1"
  mkdir -p "${case_dir}/outside"
  ln -s "${case_dir}/outside" "${case_dir}/.tools"
}

make_held_lock() {
  local case_dir="$1"
  mkdir -p "${case_dir}/.tools/bin/.bootstrap-bazelisk.lock"
}

make_non_file_install() {
  local case_dir="$1"
  mkdir -p "${case_dir}/.tools/bin/bazelisk"
}

rm -rf "${case_root}"
mkdir -p "${case_root}"

run_rejected_case \
  "symlinked-tools" \
  "Refusing to install through symlinked directory" \
  make_symlinked_tools
run_rejected_case \
  "held-lock" \
  "Another Bazelisk bootstrap is already running" \
  make_held_lock
run_rejected_case \
  "non-file-install" \
  "Refusing to replace non-file Bazelisk path" \
  make_non_file_install
