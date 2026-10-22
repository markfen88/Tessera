#!/bin/bash
# Reads the canonical JSON record emitted by install.sh; never executes it.
set -euo pipefail
root=${BUNDLE_ROOT:-/}; root=${root%/}
[[ $root = /* || -z $root ]] || { echo 'BUNDLE_ROOT must be absolute' >&2; exit 1; }
DIR=$(cd "$(dirname "$0")" && pwd)
record_dir=$root/var/lib/tessera-bundle
record=$record_dir/record.json
purge=0; keep=0; dry=0; yes=0; remove_dependents=0; step=parse; record_backup=; owned_web=0
files=(); users_modified=(); groups_created=(); users_created=(); dependencies_installed=(); packages=()
fail() { echo "uninstall: $step: $*" >&2; exit 1; }
while [ "$#" -gt 0 ]; do
  case $1 in --purge) purge=1 ;; --keep-users) keep=1 ;; --dry-run) dry=1 ;; --yes) yes=1 ;; --remove-dependents) remove_dependents=1 ;; *) fail "unknown option $1" ;; esac
  shift
done
[ "$dry" = 1 ] || [ -n "$root" ] || [ "$EUID" = 0 ] || fail 'requires root'
step=record
if [ "$dry" = 1 ] && [ "$EUID" != 0 ]; then
  echo 'installed state unknown (run as root to read the record)'
else
[ ! -L "$record_dir" ] && [ ! -L "$record" ] || fail 'record directory or file is a symlink'
[ ! -e "$record_dir" ] || [ "$(stat -c %u "$record_dir")" = "$EUID" ] || fail 'record directory has the wrong owner'
[ ! -e "$record_dir" ] || [ "$(stat -c %a "$record_dir")" = 700 ] || fail 'record directory must have mode 0700'
if [ -f "$record" ]; then
  section=''
  while IFS= read -r line; do
    if [[ $line =~ ^\"(files|users_modified|groups_created|users_created|packages|dependencies_installed)\":\ \[$ ]]; then
      section=${BASH_REMATCH[1]}
    elif [[ $line =~ ^\ \ \"([/a-zA-Z0-9_.:@+-]+)\",?$ ]]; then
      value=${BASH_REMATCH[1]}
      case $section in
        files)
          [[ $value = /etc/tessera/config.d/60-*.toml || $value = /etc/systemd/system/tesserad.service.d/*.conf || $value = /var/lib/tessera-bundle/record.json ]] && [[ $value != *..* ]] || fail 'invalid recorded path'
          files+=("$value") ;;
        users_modified) [[ $value =~ ^[a-z_][a-z0-9_-]*:(tessera|tessera-admin)$ ]] || fail 'invalid membership'; users_modified+=("$value") ;;
        groups_created) [[ $value = tessera || $value = tessera-admin ]] || fail 'invalid group'; groups_created+=("$value") ;;
        users_created) [ "$value" = tessera ] || fail 'invalid user'; users_created+=("$value") ;;
        packages)
          [[ $value = tessera || $value = tessera-alsa || $value = tessera-web ]] || fail 'invalid package'
          [ "$value" != tessera-web ] || owned_web=1 ;;
        dependencies_installed) [[ $value =~ ^[a-z0-9][a-z0-9+.-]*$ ]] || fail 'invalid dependency'; dependencies_installed+=("$value") ;;
        *) fail 'invalid record array' ;;
      esac
    elif [[ $line = '],' || $line = ']' ]]; then section=''; fi
    [ "$line" != '"purge_pending": true,' ] || purge=1
    # Preservation may be widened by --keep-users, never narrowed on resume.
    [ "$line" != '"keep_users": true,' ] || keep=1
  done < "$record"
else
  echo 'No bundle record; removing whatever tessera packages are installed; account ownership is unknown.'
fi
fi
# Read the whole record before apt purge removes /var/lib/tessera.
native_arch=$(dpkg --print-architecture)
package_status() {
  local pkg=$1 result architecture
  if ! result=$(dpkg-query -W -f='${db:Status-Status}\n' "$pkg:$native_arch" 2>/dev/null); then
    architecture=$(dpkg-query -W -f='${Architecture}\n' "$pkg" 2>/dev/null) || return 1
    [ "$architecture" = all ] || return 1
    result=$(dpkg-query -W -f='${db:Status-Status}\n' "$pkg" 2>/dev/null) || return 1
  fi
  [[ -n $result && $result != *[[:space:]]* ]] || return 1
  printf '%s' "$result"
}
for pkg in tessera-web tessera-alsa tessera; do
  [ "$pkg" != tessera-web ] || [ "$owned_web" = 1 ] || continue
  status=$(package_status "$pkg" || true)
  if [ "$status" = installed ] || { [ "$purge" = 1 ] && [ "$status" = config-files ]; }; then packages+=("$pkg"); fi
done
extras=()
if [ "${#packages[@]}" -gt 0 ] && { [ "$dry" = 0 ] || [ "$EUID" = 0 ]; }; then
  simulation=$(apt-get -s remove "${packages[@]}") || fail 'apt removal simulation failed'
  mapfile -t extras < <(printf '%s\n' "$simulation" | awk -v owned_web="$owned_web" '$1 == "Remv" {name=$2; sub(/:.*/,"",name); if(name != "tessera" && name != "tessera-alsa" && !(name == "tessera-web" && owned_web == 1)) print $2}')
fi
printf 'Plan: disable daemon; %s packages; remove recorded files; purge=%s keep-users=%s; extra removals: %s\n' "${packages[*]:-no}" "$purge" "$keep" "${extras[*]:-none}"
[ "$dry" = 0 ] || exit 0
[ "${#extras[@]}" = 0 ] || [ "$remove_dependents" = 1 ] || fail 'extra removals require --remove-dependents; --yes alone does not authorize them'
if [ "$yes" = 0 ]; then
  [ -t 0 ] || fail 'use --yes without a terminal'
  read -r -p 'Remove bundle? [y/N] ' answer
  [[ $answer = y || $answer = Y ]] || exit 1
fi
mark_purge_pending() {
  [ -f "$record" ] || return 0
  [ ! -L "$record_dir" ] && [ ! -L "$record" ] || return 1
  local temp preserve=false
  [ "$keep" = 0 ] || preserve=true
  temp=$(mktemp "$record_dir/record.json.XXXXXXXXXX")
  awk -v preserve="$preserve" '
    NR == 1 {print; print "\"purge_pending\": true,"; print "\"keep_users\": " preserve ","; next}
    !/^"(purge_pending|keep_users)":/ {print}
  ' "$record" > "$temp"
  chmod 0600 "$temp"
  mv -T "$temp" "$record"
}
scan_residue() {
  echo 'Residue (informational):'
  find "${root:-/}" -xdev \( -path "$root/proc" -o -path "$root/sys" -o -path "$root/var/lib/apt/lists" -o -path "$DIR" \) -prune -o -iname '*tessera*' -print 2>/dev/null | cat -v || true
}
restore_record() {
  if [ -n "$record_backup" ] && [ ! -e "$record" ] && [ ! -L "$record" ] &&
     [ -d "$record_dir" ] && [ ! -L "$record_dir" ]; then
    local temp
    temp=$(mktemp "$record_dir/record.json.XXXXXXXXXX") || return 1
    printf '%s\n' "$record_backup" > "$temp" || return 1
    chmod 0600 "$temp" || return 1
    mv -T "$temp" "$record"
  fi
}
on_error() {
  local rc=$?
  trap - ERR
  echo "uninstall: failed at $step (exit $rc)" >&2
  restore_record || true
  if [ "$purge" = 1 ]; then mark_purge_pending || true; fi
  if [ "$step" = record-directory ]; then scan_residue; fi
  exit 1
}
trap on_error ERR
if [ "$purge" = 1 ]; then mark_purge_pending; fi
step=service
if systemctl cat tesserad.service >/dev/null 2>&1; then systemctl disable --now tesserad; fi
step=packages
if [ "${#packages[@]}" -gt 0 ]; then
  action=remove; [ "$purge" = 0 ] || action=purge
  DEBIAN_FRONTEND=noninteractive apt-get "$action" -y "${packages[@]}"
fi
step=files
for file in "${files[@]}"; do
  [ "$file" = /var/lib/tessera-bundle/record.json ] || rm -f -- "$root$file"
done
rmdir "$root/etc/systemd/system/tesserad.service.d" 2>/dev/null || true
if [ "$purge" = 1 ]; then
  rm -rf -- "$root/etc/tessera" "$root/var/lib/tessera"
  if [ "$keep" = 0 ]; then
    step=memberships
    for membership in "${users_modified[@]}"; do
      user=${membership%:*}; group=${membership#*:}
      if getent passwd "$user" >/dev/null && getent group "$group" >/dev/null && id -nG "$user" | tr ' ' '\n' | grep -qx "$group"; then gpasswd -d "$user" "$group"; fi
    done
    step=accounts
    for user in "${users_created[@]}"; do
      if getent passwd "$user" >/dev/null; then userdel "$user"; fi
    done
    # userdel may already remove a private group.
    for group in "${groups_created[@]}"; do
      if getent group "$group" >/dev/null; then groupdel "$group"; fi
    done
  fi
fi
if [ "$purge" = 1 ]; then
  step=dependencies
  for dep in "${dependencies_installed[@]}"; do
    if [ "$(package_status "$dep" || true)" = installed ]; then apt-mark auto "$dep"; fi
  done
fi
step=reload
systemctl daemon-reload
if [ "$purge" = 0 ] && [ -f "$record" ]; then
  step=record
  temp=$(mktemp "$record_dir/record.json.XXXXXXXXXX")
  {
    printf '{\n"complete": false,\n'
    for key in packages users_modified groups_created users_created dependencies_installed; do
      printf '"%s": [\n' "$key"
      declare -n values=$key
      for value in "${values[@]}"; do printf '  "%s",\n' "$value"; done | LC_ALL=C sort -u | sed '$s/,$//'
      printf '],\n'
      unset -n values
    done
    printf '"removed": true\n}\n'
  } > "$temp"
  chmod 0600 "$temp"
  mv -T "$temp" "$record"
  scan_residue
  trap - ERR
  echo 'uninstall: complete'
  exit 0
fi
step=record-directory
if [ -d "$record_dir" ]; then
  # Retain the record in memory until checked directory removal succeeds.
  [ ! -f "$record" ] || record_backup=$(cat "$record")
  find "$record_dir" -mindepth 1 -maxdepth 1 -type f -name 'record.json.*' -delete
  rm -f -- "$record"
  rmdir "$record_dir"
fi
step=residue
scan_residue
trap - ERR
echo 'uninstall: complete'
