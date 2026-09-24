#!/usr/bin/env bash
# Install crepath from GitHub releases.
# https://github.com/SherinBloemendaal/cursor-repath

if [ -z "${BASH_VERSION:-}" ]; then
  echo "error: install.sh requires bash" >&2
  exit 1
fi

set -euo pipefail

repo="SherinBloemendaal/cursor-repath"
github="https://github.com/${repo}"

if [ -n "${NO_COLOR:-}" ] || { [ ! -t 1 ] && [ -z "${CLICOLOR_FORCE:-}" ]; }; then
  c_reset=""
  c_bold=""
  c_dim=""
  c_red=""
  c_green=""
  c_blue=""
else
  c_reset=$'\033[0m'
  c_bold=$'\033[1m'
  c_dim=$'\033[2m'
  c_red=$'\033[31m'
  c_green=$'\033[32m'
  c_blue=$'\033[34m'
fi

info() {
  printf '%s\n' "${c_bold}${c_blue}info${c_reset} $*"
}

ok() {
  printf '%s\n' "${c_bold}${c_green}ok${c_reset} $*"
}

error() {
  printf '%s\n' "${c_bold}${c_red}error${c_reset} $*" >&2
}

die() {
  error "$@"
  exit 1
}

if ! command -v curl >/dev/null 2>&1; then
  die "curl is required"
fi

download() {
  url="$1"
  dest="$2"
  curl -fsSL --retry 3 --retry-delay 2 --proto '=https' --tlsv1.2 -o "$dest" "$url"
}

normalize_version() {
  raw="$1"
  case "$raw" in
    "" ) die "empty version" ;;
    *[!A-Za-z0-9._-]*) die "invalid version: ${raw}" ;;
  esac
  case "$raw" in
    v*) printf '%s\n' "$raw" ;;
    *) printf 'v%s\n' "$raw" ;;
  esac
}

resolve_version() {
  if [ -n "${1:-}" ]; then
    normalize_version "$1"
    return
  fi
  if [ -n "${CREPATH_VERSION:-}" ]; then
    normalize_version "$CREPATH_VERSION"
    return
  fi
  latest_url="${github}/releases/latest"
  effective=""
  if ! effective="$(curl -fsSL --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' "$latest_url")"; then
    die "could not find the latest release at ${latest_url}"
  fi
  case "$effective" in
    */releases/tag/*) ;;
    *) die "unexpected latest-release URL: ${effective}" ;;
  esac
  tag="${effective##*/}"
  normalize_version "$tag"
}

target_triple() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin)
      case "$arch" in
        x86_64) printf '%s\n' "x86_64-apple-darwin" ;;
        arm64 | aarch64) printf '%s\n' "aarch64-apple-darwin" ;;
        *) die "unsupported platform: ${os} ${arch}" ;;
      esac
      ;;
    Linux)
      if [ -f /etc/alpine-release ]; then
        die "unsupported platform: musl libc (Alpine). Published Linux builds require glibc."
      fi
      case "$arch" in
        x86_64 | amd64) printf '%s\n' "x86_64-unknown-linux-gnu" ;;
        arm64 | aarch64) printf '%s\n' "aarch64-unknown-linux-gnu" ;;
        *) die "unsupported platform: ${os} ${arch}" ;;
      esac
      ;;
    *)
      die "unsupported platform: ${os} ${arch}. Use install.ps1 on Windows."
      ;;
  esac
}

sha256_file() {
  file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{ print $1 }'
  else
    die "sha256sum or shasum is required to verify the download"
  fi
}

verify_checksum() {
  file="$1"
  asset="$2"
  sums="$3"
  expected=""
  expected="$(grep -E "[[:space:]]\\*?${asset}$" "$sums" | awk 'NR == 1 { print $1 }' || true)"
  if [ -z "$expected" ]; then
    die "checksum mismatch: SHA256SUMS has no entry for ${asset}"
  fi
  expected="$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')"
  actual="$(sha256_file "$file" | tr '[:upper:]' '[:lower:]')"
  if [ "$actual" != "$expected" ]; then
    error "checksum mismatch for ${asset}"
    error "expected ${expected}"
    error "actual   ${actual}"
    exit 1
  fi
}

install_dir="${CREPATH_INSTALL:-"${HOME}/.crepath/bin"}"
install_dir="${install_dir%/}"
if ! mkdir -p "$install_dir"; then
  die "could not create ${install_dir}"
fi
install_dir="$(cd "$install_dir" && pwd)"

version="$(resolve_version "${1:-}")"
target="$(target_triple)"
asset="crepath-${target}.tar.gz"
base="${github}/releases/download/${version}"

info "installing crepath ${version} (${target})"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

archive="${tmpdir}/${asset}"
sums="${tmpdir}/SHA256SUMS"

if ! download "${base}/${asset}" "$archive"; then
  die "download failed: ${base}/${asset}"
fi
if ! download "${base}/SHA256SUMS" "$sums"; then
  die "download failed: ${base}/SHA256SUMS"
fi

verify_checksum "$archive" "$asset" "$sums"
ok "checksum verified"

extract="${tmpdir}/extract"
mkdir -p "$extract"
tar -xzf "$archive" -C "$extract"
src="${extract}/crepath"
if [ ! -f "$src" ]; then
  src="$(find "$extract" -type f -name crepath -print | head -n 1 || true)"
fi
if [ -z "$src" ] || [ ! -f "$src" ]; then
  die "archive does not contain crepath"
fi

dest="${install_dir}/crepath"
stage="${install_dir}/crepath.new"
cp "$src" "$stage"
chmod 755 "$stage"
mv -f "$stage" "$dest"
ok "installed ${dest}"

rc=""
shell_name=""
case "${SHELL:-}" in
  */zsh)
    shell_name="zsh"
    rc="${HOME}/.zshrc"
    ;;
  */bash)
    shell_name="bash"
    if [ "$(uname -s)" = "Darwin" ]; then
      rc="${HOME}/.bash_profile"
    else
      rc="${HOME}/.bashrc"
    fi
    ;;
  */fish)
    shell_name="fish"
    rc="${HOME}/.config/fish/config.fish"
    ;;
  *)
    shell_name=""
    rc=""
    ;;
esac

path_recorded=0
for candidate in \
  "${HOME}/.zshrc" \
  "${HOME}/.bashrc" \
  "${HOME}/.bash_profile" \
  "${HOME}/.config/fish/config.fish"
do
  if [ -f "$candidate" ] && grep -F -q "$install_dir" "$candidate"; then
    path_recorded=1
  fi
done

if [ "$path_recorded" -eq 0 ] && [ -n "$rc" ]; then
  mkdir -p "$(dirname "$rc")"
  {
    printf '\n'
    printf '# crepath\n'
    if [ "$shell_name" = "fish" ]; then
      printf 'fish_add_path --prepend "%s"\n' "$install_dir"
    else
      literal_path_ref="\$PATH"
      printf 'export PATH="%s:%s"\n' "$install_dir" "$literal_path_ref"
    fi
  } >>"$rc"
  ok "added ${install_dir} to ${rc}"
  info "restart your shell so PATH updates, then run crepath"
elif [ "$path_recorded" -eq 1 ]; then
  info "PATH already includes ${install_dir}"
else
  info "could not detect the shell rc file"
  info "add ${install_dir} to PATH, then run crepath"
fi

printed=""
if printed="$("$dest" --version 2>/dev/null)"; then
  printf '%s\n' "${c_dim}${printed}${c_reset}"
else
  info "crepath ${version}"
fi

ok "run crepath"
