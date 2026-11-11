#!/bin/sh
# Renders the in-repository package manifests from one source of truth.
#
# Usage:
#   packaging/render.sh                          re-render from packaging/release.env
#   packaging/render.sh VERSION CHECKSUM_DIR     record a release in packaging/release.env, then render
#   packaging/render.sh --check                  fail if the committed manifests are not what
#                                                packaging/release.env renders to (changes nothing)
#
#   VERSION       release version without the "v", for example 1.0.0
#   CHECKSUM_DIR  folder holding the release's .sha256 files, for example after
#                 gh release download v1.0.0 --repo federicolaterra/canfit --pattern '*.sha256' --dir sums
#
# Templates:  packaging/templates/canfit.rb, packaging/templates/canfit.json
# Output:     Formula/canfit.rb (Homebrew), packaging/scoop/canfit.json (Scoop)
#
# Nothing is written if a checksum is missing or malformed, or if a placeholder is left over.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
source_file="$here/release.env"

usage() {
  sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
  exit 64
}

fail() {
  echo "render.sh: $*" >&2
  exit 1
}

check_version() {
  case $1 in
    v*) echo "render.sh: give the version without the leading v ($1)" >&2; exit 64 ;;
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "render.sh: \"$1\" is not a version like 1.0.0" >&2; exit 64 ;;
  esac
}

check_hash() {
  printf '%s' "$2" | grep -Eq '^[0-9a-f]{64}$' || fail "$1 is not a lowercase SHA-256 hash: \"$2\""
}

# Reads the hash from "<hash> *<file>" or "<hash>  <file>" in CHECKSUM_DIR.
hash_from_dir() {
  file="$1/$2.sha256"
  [ -f "$file" ] || fail "missing $file"
  value=$(awk 'NR == 1 { print $1 }' "$file" | tr 'A-F' 'a-f')
  check_hash "$file" "$value"
  printf '%s' "$value"
}

# Reads KEY=value from packaging/release.env (never sourced: it is data, not a script).
read_key() {
  value=$(sed -n "s/^$1=//p" "$source_file" | tail -n 1)
  [ -n "$value" ] || fail "$source_file has no $1"
  printf '%s' "$value"
}

write_source() {
  version=$1
  sums=$2
  macos_aarch64=$(hash_from_dir "$sums" canfit-aarch64-apple-darwin.tar.xz)
  macos_x86_64=$(hash_from_dir "$sums" canfit-x86_64-apple-darwin.tar.xz)
  linux_aarch64=$(hash_from_dir "$sums" canfit-aarch64-unknown-linux-musl.tar.xz)
  linux_x86_64=$(hash_from_dir "$sums" canfit-x86_64-unknown-linux-musl.tar.xz)
  windows_zip=$(hash_from_dir "$sums" canfit-x86_64-pc-windows-msvc.zip)
  # Keep the explanatory header, replace the values.
  sed -n '/^#/p' "$source_file" > "$source_file.tmp"
  cat >> "$source_file.tmp" <<EOF
VERSION=$version
SHA256_MACOS_AARCH64=$macos_aarch64
SHA256_MACOS_X86_64=$macos_x86_64
SHA256_LINUX_AARCH64=$linux_aarch64
SHA256_LINUX_X86_64=$linux_x86_64
SHA256_WINDOWS_ZIP=$windows_zip
EOF
  mv "$source_file.tmp" "$source_file"
  echo "Recorded canfit $version in packaging/release.env"
}

# render TEMPLATE DEST: fills every placeholder from packaging/release.env.
render() {
  mkdir -p "$(dirname "$2")"
  sed -e "s/{{VERSION}}/$VERSION/g" \
      -e "s/{{SHA256_MACOS_AARCH64}}/$SHA256_MACOS_AARCH64/g" \
      -e "s/{{SHA256_MACOS_X86_64}}/$SHA256_MACOS_X86_64/g" \
      -e "s/{{SHA256_LINUX_AARCH64}}/$SHA256_LINUX_AARCH64/g" \
      -e "s/{{SHA256_LINUX_X86_64}}/$SHA256_LINUX_X86_64/g" \
      -e "s/{{SHA256_WINDOWS_ZIP}}/$SHA256_WINDOWS_ZIP/g" \
      "$1" > "$2.tmp"
  if grep -q '{{' "$2.tmp"; then
    rm -f "$2.tmp"
    fail "placeholder left in $2"
  fi
  mv "$2.tmp" "$2"
}

render_all() {
  out=$1
  VERSION=$(read_key VERSION)
  check_version "$VERSION"
  for key in SHA256_MACOS_AARCH64 SHA256_MACOS_X86_64 SHA256_LINUX_AARCH64 SHA256_LINUX_X86_64 \
             SHA256_WINDOWS_ZIP; do
    value=$(read_key "$key")
    check_hash "$key" "$value"
    eval "$key=\$value"
  done
  render "$here/templates/canfit.rb" "$out/Formula/canfit.rb"
  render "$here/templates/canfit.json" "$out/packaging/scoop/canfit.json"
}

case ${1:-} in
  -h|--help) usage ;;
  --check)
    [ "$#" -eq 1 ] || usage
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    render_all "$tmp"
    status=0
    for f in Formula/canfit.rb packaging/scoop/canfit.json; do
      if ! diff -u "$root/$f" "$tmp/$f"; then
        echo "render.sh: $f is not what packaging/release.env renders to; run packaging/render.sh" >&2
        status=1
      fi
    done
    [ "$status" -eq 0 ] && echo "Package manifests match packaging/release.env ($(read_key VERSION))."
    exit "$status"
    ;;
  "")
    render_all "$root"
    ;;
  *)
    [ "$#" -eq 2 ] || usage
    check_version "$1"
    [ -d "$2" ] || fail "$2 is not a folder"
    write_source "$1" "$2"
    render_all "$root"
    ;;
esac
echo "Rendered canfit $(read_key VERSION): Formula/canfit.rb, packaging/scoop/canfit.json"
