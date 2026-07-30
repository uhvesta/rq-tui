#!/usr/bin/env bash
set -euo pipefail

BAZELISK_VERSION="v1.29.0"
REPOSITORY_URL="https://github.com/bazelbuild/bazelisk/releases/download/${BAZELISK_VERSION}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install_dir="${repo_root}/.tools/bin"
install_path="${install_dir}/bazelisk"

case "$(uname -s)" in
  Darwin) bazelisk_os="darwin" ;;
  Linux) bazelisk_os="linux" ;;
  *)
    echo "Unsupported operating system: $(uname -s). Expected macOS or Linux." >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  arm64 | aarch64) bazelisk_arch="arm64" ;;
  x86_64 | amd64) bazelisk_arch="amd64" ;;
  *)
    echo "Unsupported architecture: $(uname -m). Expected arm64/aarch64 or x86_64." >&2
    exit 1
    ;;
esac

asset="bazelisk-${bazelisk_os}-${bazelisk_arch}"
case "${bazelisk_os}-${bazelisk_arch}" in
  darwin-amd64) expected_sha256="16c3d7aa15323a9fb69f56c7ec5733ed18bedb786680d0ba13bb12a3c8083007" ;;
  darwin-arm64) expected_sha256="cee851f726789227d5561004e9904a52be45c3efb56f8b38b6993d6adbaa0409" ;;
  linux-amd64) expected_sha256="5a408715e932c0250d28bd84555f12edbf70117de42f9181691c736eacc4a992" ;;
  linux-arm64) expected_sha256="e20e8b0f4f240091b7a55bf17b9398bd4f40ee70ae0208dff95dd4c445fb4010" ;;
esac

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    echo "A SHA-256 utility (sha256sum or shasum) is required." >&2
    return 1
  fi
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl --fail --location --retry 3 --output "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget --tries=3 --output-document="$2" "$1"
  else
    echo "curl or wget is required to download Bazelisk." >&2
    return 1
  fi
}

mkdir -p "${install_dir}"
if [[ -x "${install_path}" ]] && [[ "$(sha256_file "${install_path}")" == "${expected_sha256}" ]]; then
  echo "Bazelisk ${BAZELISK_VERSION} is already installed at ${install_path}"
else
  temporary_dir="$(mktemp -d "${TMPDIR:-/tmp}/rq-tui-bazelisk.XXXXXX")"
  trap 'rm -rf "${temporary_dir}"' EXIT
  temporary_path="${temporary_dir}/${asset}"
  download "${REPOSITORY_URL}/${asset}" "${temporary_path}"
  actual_sha256="$(sha256_file "${temporary_path}")"
  if [[ "${actual_sha256}" != "${expected_sha256}" ]]; then
    echo "Bazelisk checksum mismatch for ${asset}." >&2
    echo "Expected ${expected_sha256}, got ${actual_sha256}." >&2
    exit 1
  fi
  chmod 0755 "${temporary_path}"
  mv "${temporary_path}" "${install_path}"
  echo "Installed Bazelisk ${BAZELISK_VERSION} at ${install_path}"
fi

ln -sfn bazelisk "${install_dir}/bazel"

echo
echo "Bootstrap complete. For this shell, run:"
echo "  export PATH=\"${install_dir}:\$PATH\""
echo
echo "Then use Bazel normally; .bazelversion pins the repository's Bazel release:"
echo "  bazel test //... --lockfile_mode=error"
