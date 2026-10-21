#!/bin/bash
# Trusted bundle code. BUNDLE_ROOT relocates managed paths for isolated tests;
# BUNDLE_OS_RELEASE relocates os-release. Commands still run on the calling host.
set -euo pipefail
umask 022
DIR=$(cd "$(dirname "$0")" && pwd)
root=${BUNDLE_ROOT:-/}; root=${root%/}
[[ $root = /* || -z $root ]] || { echo 'BUNDLE_ROOT must be absolute' >&2; exit 1; }
record_dir=$root/var/lib/tessera-bundle
record=$record_dir/record.json
profile=none; card=default; restart=0; offline=0; dry=0; yes=0; reinstall=0; allow_ubuntu=0
admins=(); files=(); users_modified=(); groups_created=(); users_created=(); dependencies_installed=(); packages=(tessera tessera-alsa)
step=parse; started=0; complete=false; old_profile=; old_version=; old_commit=; old_complete=false; restart_pending=false
fail() { echo "install: $step: $*" >&2; exit 1; }
need_value() { [ "$#" -ge 2 ] || fail "missing value for $1"; }
while [ "$#" -gt 0 ]; do
  case $1 in
    --profile) need_value "$@"; profile=$2; shift 2 ;;
    --admin-user) need_value "$@"; admins+=("$2"); shift 2 ;;
    --audio-card) need_value "$@"; card=$2; shift 2 ;;
    --restart) restart=1; shift ;;
    --reinstall) reinstall=1; shift ;;
    --allow-ubuntu-24.04) allow_ubuntu=1; shift ;;
    --offline) offline=1; shift ;;
    --dry-run) dry=1; shift ;;
    --yes) yes=1; shift ;;
    *) fail "unknown option $1" ;;
  esac
done
[[ $profile =~ ^(none|speaker|ndimon)$ ]] || fail 'invalid profile'
[[ $card =~ ^[a-zA-Z0-9_:.,=+-]+$ ]] || fail 'invalid audio card'
for user in "${admins[@]}"; do [[ $user =~ ^[a-z_][a-z0-9_-]*$ ]] || fail 'invalid admin user'; done
step=manifest
[ -f "$DIR/manifest.env" ] || fail 'missing manifest.env'
# Verify the manifest itself before evaluating any of its shell assignments.
mapfile -t manifest_sums < <(awk '$2 == "./manifest.env" || $2 == "manifest.env" {print}' "$DIR/SHA256SUMS")
[ "${#manifest_sums[@]}" = 1 ] || fail 'missing or duplicate manifest checksum'
(cd "$DIR" && printf '%s\n' "${manifest_sums[0]}" | sha256sum --quiet --strict -c --ignore-missing) || fail 'manifest checksum mismatch'
# Checksums detect corruption; obtain the script and sums from a trusted release.
source "$DIR/manifest.env"
step=platform
native_arch=$(dpkg --print-architecture)
[ "$native_arch" = "$BUNDLE_ARCH" ] || fail "requires architecture $BUNDLE_ARCH"
source "${BUNDLE_OS_RELEASE:-/etc/os-release}"
case ${BUNDLE_OS_ID:-}:${BUNDLE_OS_CODENAME:-} in
  debian:trixie)
    [ "$allow_ubuntu" = 0 ] || fail 'Ubuntu opt-in requires an Ubuntu bundle'
    [ "${ID:-}" = debian ] && [ "${VERSION_CODENAME:-}" = trixie ] || fail 'requires Debian trixie'
    ;;
  ubuntu:noble)
    [ "$allow_ubuntu" = 1 ] || fail 'experimental Ubuntu requires --allow-ubuntu-24.04'
    [ "${BUNDLE_OS_VERSION_ID:-}" = 24.04 ] && [ "$BUNDLE_ARCH" = arm64 ] &&
      [ "${ID:-}" = ubuntu ] && [ "${VERSION_ID:-}" = 24.04 ] &&
      [ "${VERSION_CODENAME:-}" = noble ] || fail 'requires experimental Ubuntu 24.04 noble arm64'
    echo 'WARNING: experimental Ubuntu 24.04 arm64 bundle; live audio interoperability unverified' >&2
    ;;
  *) fail 'unsupported bundle platform' ;;
esac
systemd_version=$(systemctl --version | sed -n '1s/[^0-9]*\([0-9]*\).*/\1/p')
[[ $systemd_version =~ ^[0-9]+$ ]] && [ "$systemd_version" -ge 253 ] || fail 'requires systemd >= 253'
[ "$dry" = 1 ] || [ -n "$root" ] || [ "$EUID" = 0 ] || fail 'requires root'
step=checksum
(cd "$DIR" && sha256sum --quiet --strict -c SHA256SUMS) || fail 'checksum mismatch'
read -r -a deb_files <<< "$BUNDLE_DEBS"
for deb in "${deb_files[@]}"; do
  [[ $deb =~ ^debs/tessera(-alsa)?_[a-zA-Z0-9.+:~_-]+\.deb$ ]] || fail 'invalid deb path'
  awk -v path="./$deb" '$2 == path {found=1} END {exit !found}' "$DIR/SHA256SUMS" || fail "unchecked deb: $deb"
done
# Read only our canonical JSON record, never execute it. All values are restricted
# to safe filename/account characters; arbitrary JSON from outside is rejected.
read_record() {
  local line section='' value key
  while IFS= read -r line; do
    if [[ $line =~ ^\"(files|users_modified|groups_created|users_created|packages|dependencies_installed)\":\ \[$ ]]; then
      section=${BASH_REMATCH[1]}
    elif [[ $line =~ ^\"(version|profile|commit)\":\ \"([a-zA-Z0-9.+:~_-]+)\",?$ ]]; then
      key=${BASH_REMATCH[1]}; value=${BASH_REMATCH[2]}
      case $key in version) old_version=$value ;; profile) old_profile=$value ;; commit) old_commit=$value ;; esac
    elif [[ $line =~ ^\ \ \"([/a-zA-Z0-9_.:@+-]+)\",?$ ]]; then
      value=${BASH_REMATCH[1]}
      case $section in
        files) [[ $value = /etc/tessera/config.d/60-*.toml || $value = /etc/systemd/system/tesserad.service.d/*.conf || $value = /var/lib/tessera-bundle/record.json ]] || fail 'invalid recorded path'; [[ $value != *..* ]] || fail 'invalid recorded path'; files+=("$value") ;;
        users_modified) users_modified+=("$value") ;;
        groups_created) groups_created+=("$value") ;;
        users_created) users_created+=("$value") ;;
        packages) : ;;
        dependencies_installed) dependencies_installed+=("$value") ;;
        '') fail 'invalid record array' ;;
      esac
    elif [[ $line = '],' || $line = ']' ]]; then section=''; fi
    case $line in
      '"complete": true,') old_complete=true ;;
      '"restart_pending": true,') restart_pending=true ;;
    esac
  done < "$record"
  return 0
}
step=record
if [ "$dry" = 1 ] && [ "$EUID" != 0 ]; then
  echo 'installed state unknown (run as root to read the record)'
else
[ ! -L "$record_dir" ] && [ ! -L "$record" ] || fail 'record directory or file is a symlink'
[ ! -e "$record_dir" ] || [ "$(stat -c %u "$record_dir")" = "$EUID" ] || fail 'record directory has the wrong owner'
[ ! -e "$record_dir" ] || [ "$(stat -c %a "$record_dir")" = 700 ] || fail 'record directory must have mode 0700'
[ ! -f "$record" ] || read_record
fi
if [ -n "$old_profile" ] && [ "$profile" != "$old_profile" ]; then fail "profile is $old_profile; remove it before changing profiles"; fi
step=plan
printf 'Plan: tessera %s (%s), profile %s\n' "$BUNDLE_VERSION" "$BUNDLE_ARCH" "$profile"
printf '  apt dependency: %s\n' "${BUNDLE_APT_DEPENDS[@]}"
printf '  package: %s\n' "${deb_files[@]}"
[ "$profile" = none ] || echo "  create if absent: /etc/tessera/config.d/60-$profile.toml and profile unit drop-ins"
echo '  apply admin memberships; enable daemon; verify status; write /var/lib/tessera-bundle/record.json'
[ "$offline" = 0 ] || echo '  offline: verify all dependencies; install with --no-download'
[ "$dry" = 0 ] || { echo 'dry-run: no changes'; exit 0; }
if [ "$yes" = 0 ]; then
  [ -t 0 ] || fail 'use --yes without a terminal'
  read -r -p 'Install bundle? [y/N] ' answer
  [[ $answer = y || $answer = Y ]] || exit 1
fi
for user in "${admins[@]}"; do id "$user" >/dev/null || fail "unknown admin user: $user"; done
write_array() { local key=$1 value; shift; printf '"%s": [\n' "$key"; for value in "$@"; do printf '  "%s",\n' "$value"; done | LC_ALL=C sort -u | sed '$s/,$//'; printf '],\n'; }
write_record() {
  [ ! -L "$record_dir" ] && [ ! -L "$record" ] || return 1
  install -d -m 0700 "$record_dir"
  local temp
  temp=$(mktemp "$record_dir/record.json.XXXXXXXXXX")
  {
    printf '{\n"version": "%s",\n"commit": "%s",\n"profile": "%s",\n"complete": %s,\n"restart_pending": %s,\n' "$BUNDLE_VERSION" "$BUNDLE_COMMIT" "$profile" "$complete" "$restart_pending"
    write_array files "${files[@]}"
    write_array users_modified "${users_modified[@]}"
    write_array groups_created "${groups_created[@]}"
    write_array users_created "${users_created[@]}"
    write_array packages "${packages[@]}"
    write_array dependencies_installed "${dependencies_installed[@]}"
    printf '"timestamp": "%s"\n}\n' "$(date -u +%FT%TZ)"
  } > "$temp"
  chmod 0600 "$temp"
  mv -T "$temp" "$record"
}
on_error() { local rc=$?; trap - ERR; echo "install: failed at $step (exit $rc); packages may remain installed" >&2; if [ "$started" = 1 ]; then complete=false; write_record || true; fi; exit 1; }
trap on_error ERR
apt_install() {
  if ! apt-get install --no-remove "$@"; then
    echo 'install: refusing package removal; upgrade tessera-web and tessera-pw together with tessera to the bundle version first (or explicitly remove them beforehand)' >&2
    return 1
  fi
}
# Dependencies are stored with Debian constraints. apt takes package names; verify
# the installed versions separately (including in offline mode).
dep_names=()
check_dependencies() {
  local dep name expression op required installed
  for dep in "${BUNDLE_APT_DEPENDS[@]}"; do
    name=${dep%% *}
    installed=$(package_version "$name") || return 1
    if [[ $dep = *' ('* ]]; then
      expression=${dep#* (}; expression=${expression%)}; op=${expression%% *}; required=${expression#* }
      dpkg --compare-versions "$installed" "$op" "$required" || return 1
    fi
  done
}
for dep in "${BUNDLE_APT_DEPENDS[@]}"; do dep_names+=("${dep%% *}"); done
export DEBIAN_FRONTEND=noninteractive
package_version() {
  local name=${1%%:*} query info architecture status
  query=$name:$native_arch
  if ! info=$(dpkg-query -W -f='${Version}\n' "$query" 2>/dev/null); then
    # Architecture: all has no native qualifier. Never use an unqualified
    # multiarch lookup: dpkg-query emits one version per installed architecture.
    architecture=$(dpkg-query -W -f='${Architecture}\n' "$name" 2>/dev/null) || return 1
    [ "$architecture" = all ] || return 1
    query=$name
    info=$(dpkg-query -W -f='${Version}\n' "$query" 2>/dev/null) || return 1
  fi
  # Require exactly one nonempty version, even if a query unexpectedly emits
  # several lines. Configuration-only package remnants are not installed.
  [[ -n $info && $info != *[[:space:]]* ]] || return 1
  status=$(dpkg-query -W -f='${db:Status-Status}\n' "$query" 2>/dev/null) || return 1
  [ "$status" = installed ] || return 1
  printf '%s' "$info"
}
installed_version=$(package_version tessera || true)
alsa_version=$(package_version tessera-alsa || true)
step=version
if [ -n "$installed_version" ] && dpkg --compare-versions "$installed_version" gt "$BUNDLE_VERSION"; then fail 'refusing downgrade'; fi
if [ "$installed_version" = "$BUNDLE_VERSION" ] && [ "$old_commit" != "$BUNDLE_COMMIT" ] && [ "$reinstall" = 0 ]; then
  fail 'same version has different or unknown build provenance; use --reinstall to install the bundle build'
fi
step=dependencies
admins_satisfied=1
for user in "${admins[@]}"; do
  for group in tessera tessera-admin; do
    if ! id -nG "$user" | tr ' ' '\n' | grep -qx "$group"; then admins_satisfied=0; fi
  done
done
if [ "$old_complete" = true ] && [ "$old_version" = "$BUNDLE_VERSION" ] &&
   [ "$installed_version" = "$BUNDLE_VERSION" ] && [ "$alsa_version" = "$BUNDLE_VERSION" ] &&
   [ "$restart" = 0 ] && [ "$reinstall" = 0 ] && [ "$admins_satisfied" = 1 ]; then
  check_dependencies
  step=verification
  status=$(tesseractl status)
  printf '%s\n' "$status" | cat -v | sed -n '1p'
  [ "$restart_pending" = false ] || echo 'restart pending'
  echo 'install: already installed; verified'
  exit 0
fi
if [ "$offline" = 1 ]; then check_dependencies || fail 'offline dependency missing or too old';
else
  for dep in "${dep_names[@]}"; do
    if ! package_version "$dep" >/dev/null; then dependencies_installed+=("$dep"); fi
  done
  if [[ " ${files[*]} " != *' /var/lib/tessera-bundle/record.json '* ]]; then files+=(/var/lib/tessera-bundle/record.json); fi
  started=1; write_record
  apt_install -y --no-install-recommends "${dep_names[@]}"; check_dependencies
fi
step=packages
running=0
systemctl --quiet is-active tesserad && running=1
installed_version=$(package_version tessera || true)
# Remember account ownership before the package creates its sysusers.
for group in tessera tessera-admin; do
  if ! getent group "$group" >/dev/null; then groups_created+=("$group"); fi
done
if ! getent passwd tessera >/dev/null; then users_created+=(tessera); fi
if [[ " ${files[*]} " != *' /var/lib/tessera-bundle/record.json '* ]]; then files+=(/var/lib/tessera-bundle/record.json); fi
if [ "$running" = 1 ] && { [ "$installed_version" != "$BUNDLE_VERSION" ] || [ "$reinstall" = 1 ]; }; then restart_pending=true; fi
started=1
write_record
if [ "$installed_version" != "$BUNDLE_VERSION" ] || [ "$alsa_version" != "$BUNDLE_VERSION" ] || [ "$reinstall" = 1 ]; then
  apt_options=(); [ "$offline" = 0 ] || apt_options+=(--no-download)
  [ "$reinstall" = 0 ] || apt_options+=(--reinstall)
  paths=(); for deb in "${deb_files[@]}"; do paths+=("$DIR/$deb"); done
  apt_install -y --no-install-recommends "${apt_options[@]}" -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold "${paths[@]}"
fi
step=profile
if [ "$profile" != none ]; then
  dest=/etc/tessera/config.d/60-$profile.toml
  if [ ! -e "$root$dest" ]; then
    [ "$running" = 0 ] || restart_pending=true
    files+=("$dest"); write_record
    mkdir -p "$(dirname "$root$dest")"
    sed -e "s/@AUDIO_CARD@/$card/g" "$DIR/profiles/$profile/60-$profile.toml.in" > "$root$dest"
  fi
  shopt -s nullglob
  for source in "$DIR/profiles/$profile/tesserad.service.d/"*.conf; do
    dest=/etc/systemd/system/tesserad.service.d/$(basename "$source")
    if [ ! -e "$root$dest" ]; then
      [ "$running" = 0 ] || restart_pending=true
      files+=("$dest"); write_record; mkdir -p "$(dirname "$root$dest")"; cp "$source" "$root$dest"
    elif [[ " ${files[*]} " != *" $dest "* ]]; then
      echo 'warning: existing bridge.conf retained; verify SupplementaryGroups=audio so the daemon can open the card' >&2
    fi
  done
fi
step=memberships
for user in "${admins[@]}"; do
  for group in tessera tessera-admin; do
    if ! id -nG "$user" | tr ' ' '\n' | grep -qx "$group"; then
      users_modified+=("$user:$group"); write_record
    fi
  done
  usermod -aG tessera,tessera-admin "$user"
done
step=service
systemctl daemon-reload
if [ "$running" = 1 ]; then
  systemctl enable tesserad
  if [ "$restart" = 1 ]; then systemctl restart tesserad; restart_pending=false
  elif [ "$installed_version" != "$BUNDLE_VERSION" ] || [ "$restart_pending" = true ]; then restart_pending=true; echo 'restart pending'; fi
else systemctl enable --now tesserad; restart_pending=false; fi
step=verification
ready=0
for ((attempt=0; attempt<20; attempt++)); do
  if status=$(tesseractl status 2>&1); then ready=1; break; fi
  sleep 0.5
done
[ "$ready" = 1 ]
printf '%s\n' "$status" | cat -v | sed -n '1p'
if [ "$profile" = speaker ]; then
  bridges=$(tesseractl --json bridges)
  if [[ $bridges = *reopening* ]]; then echo 'warning: sound card unavailable; bridge reopening' >&2; fi
fi
step=record
complete=true
write_record
trap - ERR
echo 'install: complete'
