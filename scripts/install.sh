#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Install Mercury CLI from a GitHub release asset.

Usage:
  install.sh [--version <tag-or-version>] [--bin-dir <dir>] [--repo <owner/name>]

Examples:
  install.sh --version v1.0.0-beta.1
  install.sh --version 1.0.0 --bin-dir "$HOME/.local/bin"

Notes:
  - Omitting --version installs the latest non-prerelease GitHub release.
  - Explicit prerelease installs require --version (for example v1.0.0-beta.1).
  - Official archives currently exist only for macOS arm64 and Linux x86_64.
EOF
}

require_command() {
  local name="$1"
  if ! command -v "$name" >/dev/null 2>&1; then
    echo "missing required command: $name" >&2
    exit 1
  fi
}

normalize_tag() {
  local value="$1"
  if [[ "$value" == v* ]]; then
    printf '%s\n' "$value"
  else
    printf 'v%s\n' "$value"
  fi
}

resolve_latest_release_tag() {
  local repo="$1"
  python3 - "$repo" <<'PY'
import json
import sys
import urllib.request

repo = sys.argv[1]
url = f"https://api.github.com/repos/{repo}/releases/latest"
with urllib.request.urlopen(url) as response:
    payload = json.load(response)

tag_name = payload.get("tag_name")
if not tag_name:
    raise SystemExit("GitHub latest release response did not include tag_name")

print(tag_name)
PY
}

detect_archive_target() {
  local os_name
  local arch_name
  os_name="$(uname -s)"
  arch_name="$(uname -m)"

  case "$os_name" in
    Darwin)
      case "$arch_name" in
        arm64|aarch64)
          printf '%s\n' "aarch64-apple-darwin"
          ;;
        *)
          echo "unsupported macOS architecture: $arch_name" >&2
          exit 1
          ;;
      esac
      ;;
    Linux)
      case "$arch_name" in
        x86_64|amd64)
          printf '%s\n' "x86_64-unknown-linux-gnu"
          ;;
        *)
          echo "unsupported Linux architecture: $arch_name" >&2
          exit 1
          ;;
      esac
      ;;
    *)
      echo "unsupported operating system: $os_name" >&2
      echo "use a source build on this platform" >&2
      exit 1
      ;;
  esac
}

download_release_asset() {
  local url="$1"
  local output="$2"

  if command -v curl >/dev/null 2>&1; then
    curl --fail --location --silent --show-error "$url" --output "$output"
    return
  fi

  if command -v wget >/dev/null 2>&1; then
    wget -qO "$output" "$url"
    return
  fi

  echo "missing required downloader: curl or wget" >&2
  exit 1
}

install_binary() {
  local extracted_root="$1"
  local bin_dir="$2"
  local source_binary="$extracted_root/mercury-cli"
  local compat_binary="$bin_dir/mercury"
  local primary_binary="$bin_dir/mercury-cli"

  if [[ ! -f "$source_binary" ]]; then
    source_binary="$extracted_root/mercury"
  fi

  if [[ ! -f "$source_binary" ]]; then
    echo "release archive did not contain mercury-cli or mercury" >&2
    exit 1
  fi

  mkdir -p "$bin_dir"
  install -m 0755 "$source_binary" "$primary_binary"
  ln -sf "mercury-cli" "$compat_binary"

  echo "installed mercury-cli to $primary_binary"
  echo "compatibility alias available at $compat_binary"
}

main() {
  local repo="denster32/mercury-cli"
  local bin_dir="${HOME}/.local/bin"
  local requested_version=""

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --version)
        [[ $# -ge 2 ]] || {
          echo "--version requires a value" >&2
          exit 1
        }
        requested_version="$2"
        shift 2
        ;;
      --bin-dir)
        [[ $# -ge 2 ]] || {
          echo "--bin-dir requires a value" >&2
          exit 1
        }
        bin_dir="$2"
        shift 2
        ;;
      --repo)
        [[ $# -ge 2 ]] || {
          echo "--repo requires a value" >&2
          exit 1
        }
        repo="$2"
        shift 2
        ;;
      --help|-h)
        usage
        exit 0
        ;;
      *)
        echo "unknown argument: $1" >&2
        usage >&2
        exit 1
        ;;
    esac
  done

  require_command tar
  require_command install
  require_command python3

  local tag
  if [[ -n "$requested_version" ]]; then
    tag="$(normalize_tag "$requested_version")"
  else
    tag="$(resolve_latest_release_tag "$repo")"
  fi

  local version="${tag#v}"
  local archive_target
  archive_target="$(detect_archive_target)"
  local archive_name="mercury-${version}-${archive_target}.tar.gz"
  local asset_url="https://github.com/${repo}/releases/download/${tag}/${archive_name}"

  local temp_dir
  temp_dir="$(mktemp -d)"
  trap 'rm -rf "$temp_dir"' EXIT

  local archive_path="$temp_dir/$archive_name"
  download_release_asset "$asset_url" "$archive_path"
  tar -xzf "$archive_path" -C "$temp_dir"
  install_binary "$temp_dir/mercury-${version}-${archive_target}" "$bin_dir"

  cat <<EOF

Release tag: $tag
Archive target: $archive_target
Benchmark publication bundle, when present, is published separately as:
  mercury-benchmarks-${version}.tar.gz

Add $bin_dir to PATH if needed:
  export PATH="$bin_dir:\$PATH"
EOF
}

main "$@"
