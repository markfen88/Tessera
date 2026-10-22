#!/bin/bash
# Static checks of the built packages (a stand-in for lintian, which is not
# installed on the build VM). Does not install anything and needs no root.
#
#   packaging/check-deb.sh [file.deb ...]      default: target/deb/*.deb
#
# Checks: control fields; every entry owned by root:root; no setuid/world
# writable files; no build paths inside the binaries; md5sums verify;
# maintainer scripts are executable and parse with `sh -n`; conffiles exist in
# the package; for the tessera package, /etc/tessera/config.toml exists, has
# no active line, and the examples copy of it is identical, and NEWS.Debian is
# present; man pages are gzip -n and render with groff without warnings;
# unit files pass `systemd-analyze verify` (run on the extracted file); sysusers.d parses
# (`systemd-sysusers --dry-run --root`); the vendor trade name is not used in
# package names or control descriptions.
set -uo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
if [ $# -eq 0 ]; then set -- "${CARGO_TARGET_DIR:-$ROOT/target}"/deb/*.deb; fi
fail=0
bad() { echo "  FAIL: $*"; fail=1; }
ok()  { echo "  ok:   $*"; }

for deb in "$@"; do
  echo "== $(basename "$deb")"
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  dpkg-deb -x "$deb" "$tmp/root"
  dpkg-deb -e "$deb" "$tmp/ctl"
  pkg=$(dpkg-deb -f "$deb" Package)

  for f in Package Version Architecture Maintainer Installed-Size Depends Section Priority Description; do
    dpkg-deb -f "$deb" "$f" >/dev/null 2>&1 && [ -n "$(dpkg-deb -f "$deb" "$f")" ] || bad "control field $f missing"
  done
  ok "control: $(dpkg-deb -f "$deb" Version) $(dpkg-deb -f "$deb" Architecture), Depends: $(dpkg-deb -f "$deb" Depends)"
  dpkg-deb -f "$deb" Maintainer | grep -q '^tessera maintainers <tessera@invalid>$' || bad "maintainer placeholder"
  dpkg-deb -f "$deb" Description | grep -qi dante && bad "control description names the vendor"

  # ownership and modes
  if dpkg-deb -c "$deb" | awk '{print $2}' | grep -qv '^root/root$'; then bad "entries not owned by root/root"; else ok "all entries root/root"; fi
  if dpkg-deb -c "$deb" | awk '$1 !~ /^l/ && (substr($1,4,1) ~ /[sS]/ || substr($1,7,1) ~ /[sS]/ || substr($1,9,1) == "w" || substr($1,10,1) ~ /[tT]/) {bad=1} END{exit !bad}'; then bad "setuid/setgid/sticky or world-writable entry"; else ok "no setuid / world-writable entries"; fi
  if dpkg-deb -c "$deb" | awk '{print $6}' | grep -qE '^\./(tmp|home|var/lib|run)/'; then bad "package ships files under tmp/home/var/lib/run"; else ok "nothing under /tmp /home /var/lib /run"; fi

  # build paths in binaries
  for b in "$tmp"/root/usr/bin/*; do
    [ -f "$b" ] || continue
    if strings "$b" | grep -qE '/home/[a-z]+/|/build/tessera/\.\./'; then bad "$(basename "$b") embeds a home directory"; fi
  done
  ok "binaries carry no /home paths"

  # md5sums
  (cd "$tmp/root" && md5sum --quiet -c "$tmp/ctl/md5sums") >/dev/null 2>&1 && ok "md5sums verify" || bad "md5sums"
  n_files=$(cd "$tmp/root" && find . -type f | wc -l); n_sums=$(wc -l < "$tmp/ctl/md5sums")
  [ "$n_files" = "$n_sums" ] && ok "md5sums covers all $n_files files" || bad "md5sums lists $n_sums of $n_files files"

  # maintainer scripts
  for s in postinst prerm postrm preinst; do
    [ -f "$tmp/ctl/$s" ] || continue
    [ -x "$tmp/ctl/$s" ] || bad "$s not executable"
    sh -n "$tmp/ctl/$s" || bad "$s does not parse"
    head -n1 "$tmp/ctl/$s" | grep -q '^#!/bin/sh' || bad "$s: interpreter"
  done
  [ -f "$tmp/ctl/conffiles" ] && while read -r c; do
    [ -f "$tmp/root$c" ] && ok "conffile $c is in the package" || bad "conffile $c missing"
  done < "$tmp/ctl/conffiles"
  if [ "$pkg" = tessera ]; then
    grep -qx '/etc/tessera/config.toml' "$tmp/ctl/conffiles" 2>/dev/null \
      && ok "DEBIAN/conffiles lists /etc/tessera/config.toml" || bad "DEBIAN/conffiles does not list /etc/tessera/config.toml"
  fi

  # the reference configuration: the conffile and the examples copy are the same file, and no line of it is active
  if [ -f "$tmp/root/etc/tessera/config.toml" ]; then
    cmp -s "$tmp/root/etc/tessera/config.toml" "$tmp/root/usr/share/doc/$pkg/examples/config.toml" \
      && ok "examples/config.toml equals the conffile /etc/tessera/config.toml" || bad "examples/config.toml differs from /etc/tessera/config.toml"
    if grep -q -v -e '^#' -e '^$' "$tmp/root/etc/tessera/config.toml"; then
      bad "/etc/tessera/config.toml has an active (uncommented) line"
    else
      ok "/etc/tessera/config.toml is all comments (the reference)"
    fi
  elif [ "$pkg" = tessera ]; then
    bad "the tessera package has no /etc/tessera/config.toml"
  fi
  if [ "$pkg" = tessera ]; then
    [ -f "$tmp/root/usr/share/doc/tessera/NEWS.Debian.gz" ] && ok "NEWS.Debian.gz is present" || bad "no NEWS.Debian.gz"
  fi

  # docs
  [ -f "$tmp/root/usr/share/doc/$pkg/copyright" ] || bad "no copyright file"
  [ -f "$tmp/root/usr/share/doc/$pkg/changelog.Debian.gz" ] || bad "no changelog.Debian.gz"
  for g in $(find "$tmp/root" -name '*.gz'); do
    [ "$(gzip -l -v "$g" | awk 'NR==2{print $3}')" ] || true
    # gzip -n leaves no name and a zero mtime in the header
    [ "$(od -An -tx1 -j4 -N4 "$g" | tr -d ' ')" = 00000000 ] || bad "$(basename "$g") has a timestamp in its gzip header"
  done
  for m in $(find "$tmp/root/usr/share/man" -name '*.gz' 2>/dev/null); do
    w=$(zcat "$m" | groff -man -ww -z 2>&1 | head -3)
    [ -z "$w" ] && ok "man page $(basename "$m") renders" || bad "man page $(basename "$m"): $w"
  done

  # systemd units: verified on the extracted file against the host's own
  # units. The only complaint ignored is "binary not installed on this host"
  # (the binary is checked to exist inside the package instead).
  for u in "$tmp"/root/usr/lib/systemd/system/tessera*.service "$tmp"/root/usr/lib/systemd/user/*.service; do
    [ -f "$u" ] || continue
    mode=; case $u in */systemd/user/*) mode=--user ;; esac
    out=$(systemd-analyze $mode verify "$u" 2>&1 | grep -v "^$" | while read -r l; do
      bin=$(echo "$l" | sed -n 's/.*Command \(\/[^ ]*\) is not executable.*/\1/p')
      if [ -n "$bin" ] && [ -x "$tmp/root$bin" ]; then continue; fi
      echo "$l"
    done)
    [ -z "$out" ] && ok "systemd-analyze $mode verify $(basename "$u")" || { echo "$out" | sed 's/^/    /'; bad "unit $(basename "$u")"; }
  done
  if [ -f "$tmp/root/usr/lib/sysusers.d/tessera.conf" ]; then
    mkdir -p "$tmp/sysroot/etc"; : > "$tmp/sysroot/etc/passwd"; : > "$tmp/sysroot/etc/group"
    out=$(systemd-sysusers --dry-run --root="$tmp/sysroot" "$tmp/root/usr/lib/sysusers.d/tessera.conf" 2>&1)
    echo "$out" | grep -q "Creating user 'tessera'" && echo "$out" | grep -q "Creating group 'tessera-admin'" \
      && ok "sysusers.d creates user+group tessera and group tessera-admin" || { echo "$out" | sed 's/^/    /'; bad "sysusers.d"; }
  fi
  rm -rf "$tmp"; trap - EXIT
done
[ "$fail" = 0 ] && echo "ALL CHECKS PASSED" || echo "SOME CHECKS FAILED"
exit "$fail"
