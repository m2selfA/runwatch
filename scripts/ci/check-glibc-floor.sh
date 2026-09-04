#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 <max-glibc-version> <elf> [elf ...]" >&2
  exit 2
fi

floor="$1"
shift

if ! printf '%s\n' "$floor" | grep -Eq '^[0-9]+([.][0-9]+)*$'; then
  echo "invalid glibc floor: $floor" >&2
  exit 2
fi

command -v readelf >/dev/null 2>&1 || {
  echo "readelf is required for the glibc compatibility gate" >&2
  exit 2
}

for binary in "$@"; do
  if [[ ! -f "$binary" ]]; then
    echo "missing ELF binary: $binary" >&2
    exit 1
  fi

  versions="$({ readelf --version-info "$binary" || true; } \
    | grep -oE 'GLIBC_[0-9]+([.][0-9]+)*' \
    | sed 's/^GLIBC_//' \
    | sort -Vu || true)"

  if [[ -z "$versions" ]]; then
    echo "$binary: no GLIBC version requirements found; refusing to guess compatibility" >&2
    exit 1
  fi

  max_required="$(printf '%s\n' "$versions" | tail -n 1)"
  newest="$(printf '%s\n%s\n' "$floor" "$max_required" | sort -V | tail -n 1)"
  if [[ "$newest" != "$floor" ]]; then
    echo "$binary requires GLIBC_$max_required, newer than allowed GLIBC_$floor" >&2
    exit 1
  fi

  echo "$binary: maximum required symbol version GLIBC_$max_required <= GLIBC_$floor"
done
