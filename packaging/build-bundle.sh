#!/bin/bash
# Native builds only. BUNDLE_DEB_DIR assembles prebuilt debs; BUNDLE_CHECK_DEB
# selects an optional package checker. Output: target/bundle/.
set -euo pipefail
umask 022
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
bundle_os=debian; bundle_codename=trixie; bundle_version_id=13
if [ "${1:-}" = --allow-ubuntu-24.04 ]; then
  [ "$#" = 1 ] || { echo 'unexpected build arguments' >&2; exit 1; }
  source "${BUNDLE_OS_RELEASE:-/etc/os-release}"
  [ "${ID:-}" = ubuntu ] && [ "${VERSION_ID:-}" = 24.04 ] &&
    [ "${VERSION_CODENAME:-}" = noble ] && [ "$(dpkg --print-architecture)" = arm64 ] || {
      echo 'experimental build requires native Ubuntu 24.04 noble arm64' >&2; exit 1;
    }
  bundle_os=ubuntu; bundle_codename=noble; bundle_version_id=24.04
elif [ "$#" != 0 ]; then
  echo 'unexpected build arguments' >&2; exit 1
elif [ -z "${BUNDLE_DEB_DIR:-}" ]; then
  source "${BUNDLE_OS_RELEASE:-/etc/os-release}"
  [ "${ID:-}" = debian ] && [ "${VERSION_CODENAME:-}" = trixie ] || {
    echo 'requires Debian trixie; Ubuntu 24.04 arm64 needs --allow-ubuntu-24.04' >&2; exit 1;
  }
fi
if [ -z "${BUNDLE_DEB_DIR:-}" ]; then
  host=$(rustc -vV | sed -n 's/^host: //p')
  [ "${TARGET:-$host}" = "$host" ] || { echo 'bundle builds must be native' >&2; exit 1; }
  TESSERA_DAEMON_FEATURES=alsa-bridge SKIP_PW=1 bash "$ROOT/packaging/build-deb.sh"
fi
DEB_DIR=${BUNDLE_DEB_DIR:-$TARGET_DIR/deb}
shopt -s nullglob
debs=("$DEB_DIR"/tessera_*.deb "$DEB_DIR"/tessera-alsa_*.deb)
[ "${#debs[@]}" = 2 ] || { echo 'expected exactly tessera and tessera-alsa debs' >&2; exit 1; }
if [ -n "${BUNDLE_CHECK_DEB:-}" ]; then
  "$BUNDLE_CHECK_DEB" "${debs[@]}"
else
  for deb in "${debs[@]}"; do dpkg-deb --info "$deb" >/dev/null; done
fi
version=$(dpkg-deb -f "${debs[0]}" Version)
arch=$(dpkg-deb -f "${debs[0]}" Architecture)
[[ $version =~ ^[0-9A-Za-z.+:~-]+$ && $arch =~ ^(amd64|arm64)$ ]] || exit 1
[ "$bundle_os" != ubuntu ] || [ "$arch" = arm64 ] || exit 1
# The normal build-deb path also assembles the portal; only these two debs enter the bundle.
for deb in "${debs[@]}"; do
  [ "$(dpkg-deb -f "$deb" Version)" = "$version" ]
  [ "$(dpkg-deb -f "$deb" Architecture)" = "$arch" ]
done
mapfile -t depends < <({
  for deb in "${debs[@]}"; do dpkg-deb -f "$deb" Depends | tr ',' '\n'; done
  printf '\navahi-daemon\nalsa-utils\n'
} | sed 's/^[[:space:]]*//;s/[[:space:]]*$//' | sed '/^$/d; /^tessera (/d; /^systemd\([ (]\|$\)/d; /^init-system-helpers\([ (]\|$\)/d' | LC_ALL=C sort -u)
# dpkg emits dependency syntax, not arbitrary JSON strings; reject unsupported fields.
declare -A selected=() operators=() versions=()
for dep in "${depends[@]}"; do
  [[ $dep =~ ^[a-z0-9][a-z0-9+.:~-]*(\ \((\>\=|\<\=|\=|\>\>|\<\<)\ [a-zA-Z0-9.+:~_-]+\))?$ ]] || {
    echo "unsupported dependency expression: $dep" >&2; exit 1;
  }
  package=${dep%% *}; op=; required=
  if [[ $dep = *' ('* ]]; then
    expression=${dep#* (}; expression=${expression%)}
    op=${expression%% *}; required=${expression#* }
  fi
  if [ -z "${selected[$package]+present}" ] || [ -z "${operators[$package]}" ]; then
    selected[$package]=$dep; operators[$package]=$op; versions[$package]=$required
  elif [ -z "$op" ] || [ "$dep" = "${selected[$package]}" ]; then
    :
  elif [ "$op" = '>=' ] && [ "${operators[$package]}" = '>=' ]; then
    if dpkg --compare-versions "$required" gt "${versions[$package]}"; then
      selected[$package]=$dep; versions[$package]=$required
    fi
  else
    # Do not silently weaken equality/upper-bound expressions if they appear.
    echo "cannot combine dependency constraints for $package" >&2; exit 1
  fi
done
mapfile -t depends < <(printf '%s\n' "${selected[@]}" | LC_ALL=C sort)
OUT=$TARGET_DIR/bundle
mkdir -p "$OUT"
name=tessera-bundle_${version}_${arch}
[ "$bundle_os" != ubuntu ] || name+=_ubuntu-noble-experimental
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
stage=$work/$name
mkdir -p "$stage/debs"
cp -a packaging/bundle/. "$stage/"
cp docs/integration-contract.json "$stage/integration-contract.json"
rm -f "$stage/manifest.schema.md"
cp "${debs[@]}" "$stage/debs/"
chmod 0755 "$stage/install.sh" "$stage/uninstall.sh"
commit=$(git rev-parse HEAD)
built=$(date -u -d "@$SOURCE_DATE_EPOCH" +%FT%TZ)
json_array() { local sep='' value; printf '['; for value in "$@"; do printf '%s"%s"' "$sep" "$value"; sep=,; done; printf ']'; }
{
  printf '{"name":"tessera","version":"%s","arch":"%s","built":"%s","commit":"%s",\n' "$version" "$arch" "$built" "$commit"
  printf '"os":{"id":"%s","codename":"%s","version_id":"%s","systemd_min":253},"apt_depends":' "$bundle_os" "$bundle_codename" "$bundle_version_id"
  json_array "${depends[@]}"
  printf ',\n"debs":['
  sep=''
  for deb in "${debs[@]}"; do
    printf '%s{"file":"debs/%s","package":"%s","sha256":"%s"}' "$sep" "$(basename "$deb")" "$(dpkg-deb -f "$deb" Package)" "$(sha256sum "$deb" | cut -d' ' -f1)"
    sep=,
  done
  printf '],"profiles":["speaker","ndimon"],"min_ndimon":"1.3.0"}\n'
} > "$stage/manifest.json"
{
  printf 'BUNDLE_NAME=tessera\nBUNDLE_VERSION=%q\nBUNDLE_ARCH=%q\nBUNDLE_BUILT=%q\nBUNDLE_COMMIT=%q\n' "$version" "$arch" "$built" "$commit"
  printf 'BUNDLE_OS_ID=%s\nBUNDLE_OS_CODENAME=%s\nBUNDLE_OS_VERSION_ID=%s\nBUNDLE_SYSTEMD_MIN=253\nBUNDLE_MIN_NDIMON=1.3.0\nBUNDLE_PROFILES="speaker ndimon"\n' "$bundle_os" "$bundle_codename" "$bundle_version_id"
  printf 'BUNDLE_DEBS=%q\n' "debs/$(basename "${debs[0]}") debs/$(basename "${debs[1]}")"
  printf 'BUNDLE_DEB_PACKAGES=('
  for deb in "${debs[@]}"; do printf ' %q' "$(dpkg-deb -f "$deb" Package)"; done
  printf ' )\nBUNDLE_DEB_SHA256=('
  for deb in "${debs[@]}"; do printf ' %q' "$(sha256sum "$deb" | cut -d' ' -f1)"; done
  printf ' )\n'
  printf 'BUNDLE_APT_DEPENDS=('
  printf ' %q' "${depends[@]}"
  printf ' )\n'
} > "$stage/manifest.env"
(cd "$stage" && find . -type f ! -name SHA256SUMS -print0 | LC_ALL=C sort -z | xargs -0 sha256sum > SHA256SUMS)
find "$stage" -type d -exec chmod 0755 {} +
find "$stage" -type f ! -name install.sh ! -name uninstall.sh -exec chmod 0644 {} +
tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner -C "$work" -czf "$OUT/$name.tar.gz" "$name"
echo "$OUT/$name.tar.gz"
