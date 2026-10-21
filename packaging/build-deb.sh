#!/bin/bash
# Build the Debian packages of tessera: tessera, tessera-pw, tessera-alsa,
# tessera-web.
#
#   packaging/build-deb.sh            native build for this machine
#   TARGET=aarch64-unknown-linux-gnu packaging/build-deb.sh   cross build
#
# Output: target/deb/<package>_<version>_<arch>.deb (+ SHA256SUMS).
# Also checks that dist/etc/tessera/config.toml equals the reference generated from the schema (see below).
# Needs: cargo, dpkg-deb, dpkg-shlibdeps (dpkg-dev), gzip, xz, GNU date/find.
# No fakeroot, no debhelper, no cargo-deb, no Python. Crate manifests are not
# touched: everything the packages need is in dist/ and packaging/.
#
# Environment (all optional):
#   TARGET              Rust target triple (default: the host)
#   SOURCE_DATE_EPOCH   timestamp for every file in the packages
#                       (default: committer time of HEAD)
#   TESSERA_DEB_REVISION  Debian revision (default: first-parent release count)
#   NO_BUILD=1          reuse the binaries under target/ (assemble only)
#   SYSTEMD_MIN=253     minimum systemd in Depends (Type=notify-reload needs 253;
#                       see docs/user-guide.md for older systems)
#   SKIP_PW=1           do not build/package tessera-pw (needed for cross
#                       builds: libspa-sys runs bindgen on the HOST headers)
#   TESSERA_DAEMON_FEATURES  comma-separated cargo features of tesserad, for example
#                       alsa-bridge (the in-daemon sound-card bridge; the package then
#                       depends on libasound2, found by dpkg-shlibdeps). Default: none.
#   SKIP_ALSA=1         do not build the ALSA plugin (tessera-alsa package)
#   CARGO_TARGET_DIR    respected
#
# Reproducibility: binaries are built with --remap-path-prefix for the source
# tree, the cargo home and the Rust sysroot, and with symbols stripped; the
# .deb assembly uses fixed timestamps, owner root:root, sorted file lists and
# `gzip -n`. Building the same tree twice gives identical .deb files.
set -euo pipefail
umask 022

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

MAINTAINER='tessera maintainers <tessera@invalid>'
VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\([^"]*\)".*/\1/p}' Cargo.toml)
[ -n "$VERSION" ] || { echo "cannot read the workspace version from Cargo.toml" >&2; exit 1; }
have_origin_main=0
if git remote get-url origin >/dev/null 2>&1 && git show-ref --verify --quiet refs/remotes/origin/main 2>/dev/null; then
  have_origin_main=1
fi
if [ "$have_origin_main" = 0 ] && [ -z "${TESSERA_DEB_REVISION:-}" ]; then
  echo 'no origin/main to compare: set TESSERA_DEB_REVISION explicitly' >&2; exit 2
fi
on_main=0
if [ "$have_origin_main" = 1 ]; then git merge-base --is-ancestor HEAD origin/main 2>/dev/null && on_main=1; fi
if [ "${TESSERA_RELEASE:-0}" = 1 ] && [ "$on_main" != 1 ]; then
  echo 'TESSERA_RELEASE=1 requires HEAD to be an ancestor of origin/main' >&2; exit 1
fi
if [ -n "${TESSERA_DEB_REVISION:-}" ]; then
  REVISION=$TESSERA_DEB_REVISION
else
  if [ "$(git rev-parse --is-shallow-repository)" = true ]; then
    echo 'shallow checkout: fetch full history or set TESSERA_DEB_REVISION' >&2; exit 1
  fi
  anchor=HEAD
  # Preview commits must not advance the release counter: squash merging them
  # adds only one first-parent commit to main.
  if [ "$on_main" = 0 ]; then anchor=$(git merge-base HEAD origin/main); fi
  REVISION=$(git rev-list --count --first-parent "$anchor")
fi
if [ "$on_main" = 0 ]; then
  preview_sha=$(git rev-parse --short=7 HEAD 2>/dev/null || true)
  # A source tarball has no commit identity; its revision must be explicit.
  [ -z "$preview_sha" ] || REVISION+=~g$preview_sha
fi
# Debian sorts ~ before the final version; SemVer's prerelease hyphen would
# otherwise make 0.2.0-beta compare newer than the eventual 0.2.0 release.
DEBIAN_UPSTREAM=${VERSION/-/\~}
DEBVER="$DEBIAN_UPSTREAM-$REVISION"
if [ "${1:-}" = --print-version ]; then printf '%s\n' "$DEBVER"; exit 0; fi

HOST_TRIPLE=$(rustc -vV | sed -n 's/^host: //p')
TARGET=${TARGET:-$HOST_TRIPLE}
case $TARGET in
  x86_64-unknown-linux-gnu)         ARCH=amd64; MULTIARCH=x86_64-linux-gnu ;;
  aarch64-unknown-linux-gnu)        ARCH=arm64; MULTIARCH=aarch64-linux-gnu ;;
  armv7-unknown-linux-gnueabihf)    ARCH=armhf; MULTIARCH=arm-linux-gnueabihf ;;
  *) echo "unsupported target $TARGET (amd64, arm64 and armhf Linux/glibc only)" >&2; exit 1 ;;
esac
NATIVE=0
[ "$TARGET" = "$HOST_TRIPLE" ] && NATIVE=1

if [ -z "${SOURCE_DATE_EPOCH:-}" ]; then
  SOURCE_DATE_EPOCH=$(git log -1 --format=%ct 2>/dev/null || date +%s)
fi
export SOURCE_DATE_EPOCH
SDE=$SOURCE_DATE_EPOCH

TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
if [ "$NATIVE" = 1 ] && [ -z "${FORCE_TARGET_FLAG:-}" ]; then
  REL=$TARGET_DIR/release
  TARGET_FLAG=()
else
  REL=$TARGET_DIR/$TARGET/release
  TARGET_FLAG=(--target "$TARGET")
fi
OUT=$TARGET_DIR/deb
STAGE=$OUT/stage

say() { printf 'build-deb: %s\n' "$*" >&2; }

# ---------------------------------------------------------------- binaries
if [ "${NO_BUILD:-0}" != 1 ]; then
  SYSROOT=$(rustc --print sysroot)
  CARGO_HOME_DIR=${CARGO_HOME:-$HOME/.cargo}
  export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$ROOT=/build/tessera --remap-path-prefix=$CARGO_HOME_DIR=/build/cargo --remap-path-prefix=$SYSROOT=/build/rustc -C strip=symbols"
  say "building tesserad, tesseractl and tessera-web ($TARGET, release, --locked)"
  FEATURE_FLAG=()
  if [ -n "${TESSERA_DAEMON_FEATURES:-}" ]; then
    FEATURE_FLAG=(--features "$(printf '%s' "$TESSERA_DAEMON_FEATURES" | tr -d ' ' | sed 's|[^,][^,]*|tesserad/&|g; s|tesserad/tesserad/|tesserad/|g')")
    say "tesserad features: $TESSERA_DAEMON_FEATURES"
  fi
  cargo build --release --locked "${TARGET_FLAG[@]}" -p tesserad -p tesseractl -p tessera-web -p tessera-network "${FEATURE_FLAG[@]}"
  if [ "${SKIP_PW:-0}" != 1 ]; then
    say "building tessera-pw (--features pw)"
    cargo build --release --locked "${TARGET_FLAG[@]}" -p tessera-backend-pipewire --features pw
  fi
  if [ "${SKIP_ALSA:-0}" != 1 ]; then
    # The plugin is built without the daemon's `bridge` feature (it needs no `alsa` crate), so it is its own cargo invocation;
    # alsa-lib loads it under the name libasound_module_pcm_<type>.so.
    say "building the ALSA plugin (--no-default-features --features plugin)"
    cargo build --release --locked "${TARGET_FLAG[@]}" -p tessera-backend-alsa --no-default-features --features plugin
    cp "$REL/libtessera_backend_alsa.so" "$REL/libasound_module_pcm_tessera.so"
  fi
fi
# The reference configuration is generated from the schema (crates/tessera-state/src/schema.rs). The one committed copy,
# dist/etc/tessera/config.toml, is what the package installs below (as /etc/tessera/config.toml and again as
# /usr/share/doc/tessera/examples/config.toml; there is no second file), so a build is reproducible. Here it is
# regenerated from the workspace and a stale copy stops the build. This is a host build of one example of tessera-state
# (also in a cross build: it runs on this machine, so no --target). It needs no network once the dependencies are in
# the cargo cache, which the builds above guarantee; set CARGO_NET_OFFLINE=true to enforce it. The tessera-state test
# `dist` checks the same.
if [ "${NO_BUILD:-0}" != 1 ]; then
  say "checking dist/etc/tessera/config.toml against the schema"
  REFCFG=$OUT/config.toml.generated
  mkdir -p "$OUT"
  cargo run --release --locked -q -p tessera-state --example reference_config > "$REFCFG"
  if ! cmp -s "$REFCFG" dist/etc/tessera/config.toml; then
    diff -u dist/etc/tessera/config.toml "$REFCFG" | head -40 >&2 || true
    echo "dist/etc/tessera/config.toml is stale (it differs from the schema). Regenerate it:" >&2
    echo "  cargo run -q -p tessera-state --example reference_config > dist/etc/tessera/config.toml" >&2
    exit 1
  fi
  rm -f "$REFCFG"
fi

for b in tesserad tesseractl; do
  [ -x "$REL/$b" ] || { echo "missing $REL/$b" >&2; exit 1; }
done
HAVE_PW=0
if [ "${SKIP_PW:-0}" != 1 ] && [ -x "$REL/tessera-pw" ]; then HAVE_PW=1; fi

# The ALSA plugin is written by another crate. Package it only if it exists.
ALSA_SO=
if [ "${SKIP_ALSA:-0}" != 1 ]; then
  for c in "$REL/libasound_module_pcm_tessera.so" "$REL/deps/libasound_module_pcm_tessera.so"; do
    [ -f "$c" ] && { ALSA_SO=$c; break; }
  done
fi
HAVE_ALSA=0
if [ -n "$ALSA_SO" ]; then HAVE_ALSA=1; fi

# ---------------------------------------------------------------- helpers
# Dependencies from the shared libraries a binary needs (dpkg-shlibdeps needs
# a debian/control file in the current directory, even an empty one).
shlibdeps() {
  if [ "$NATIVE" != 1 ]; then
    # cross builds: hand-written, see packaging/README. libasound2 is added for every file whose ELF
    # header says it needs it (the ALSA plugin, tesserad built with the alsa-bridge feature): the host
    # readelf reads foreign-architecture files, so this does not depend on how the binary was built.
    local extra='' f
    for f in "$@"; do
      if readelf -d "$f" 2>/dev/null | grep -q '(NEEDED).*\[libasound\.so\.2\]'; then extra=', libasound2'; fi
    done
    echo "libc6, libgcc-s1$extra"
    return
  fi
  local d args=() out
  d=$(mktemp -d)
  mkdir "$d/debian"; : > "$d/debian/control"
  for f in "$@"; do args+=("-e$f"); done
  out=$(cd "$d" && dpkg-shlibdeps -O "${args[@]}" 2>"$d/err" | sed -n 's/^shlibs:Depends=//p') || { cat "$d/err" >&2; exit 1; }
  rm -rf "$d"
  [ -n "$out" ] || { echo "dpkg-shlibdeps found no dependencies" >&2; exit 1; }
  echo "$out"
}

# Plain-text man page from `--help` (no Rust crate involved).
#   mkman <name> <section> <bin> <summary> <outfile> [subcommand...]
roff() { sed -e 's/\\/\\e/g' -e 's/-/\\-/g' -e "s/^\\([.']\\)/\\\\\\&\\1/"; }
mkman() {
  local name=$1 sec=$2 bin=$3 summary=$4 out=$5 sub
  shift 5
  {
    printf '.TH %s %s "" "tessera %s" "tessera"\n' "$(echo "$name" | tr a-z A-Z)" "$sec" "$VERSION"
    printf '.SH NAME\n%s \\- %s\n' "$name" "$summary"
    printf '.SH DESCRIPTION\nThis page is generated from the program'"'"'s own help text.\n'
    printf 'The user guide (/usr/share/doc/tessera/user\\-guide.md) has the long form.\n'
    printf '.SH OPTIONS AND USAGE\n.nf\n'
    "$bin" --help | roff
    printf '.fi\n'
    for sub in "$@"; do
      printf '.SH COMMAND: %s\n.nf\n' "$(echo "$sub" | tr a-z A-Z)"
      # shellcheck disable=SC2086
      "$bin" $sub --help | roff
      printf '.fi\n'
    done
    printf '.SH SEE ALSO\ntesserad(8), tesseractl(1), tessera\\-pw(1)\n'
  } | gzip -9n > "$out"
}

# Normalise a staged tree: modes, timestamps.
normalise() {
  find "$1" -type d -exec chmod 0755 {} +
  find "$1" -type f ! -perm -u+x -exec chmod 0644 {} +
  find "$1" -exec touch -h -d "@$SDE" {} +
}

mkcopyright() { # <pkgdir> <pkg>
  local d=$1/usr/share/doc/$2
  mkdir -p "$d"
  cat > "$d/copyright" <<'EOF'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: tessera
Source: https://github.com/markfen88/Tessera

Files: *
Copyright: the tessera authors
License: MPL-2.0

License: MPL-2.0
 This Source Code Form is subject to the terms of the Mozilla Public
 License, v. 2.0. If a copy of the MPL was not distributed with this file,
 You can obtain one at https://mozilla.org/MPL/2.0/.
 .
 On Debian systems the full text is in /usr/share/common-licenses/MPL-2.0.

Comment: The binaries statically link Rust crates under permissive licences
 (MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0, CC0 and MPL-2.0). The allowed
 set is enforced by `cargo deny check` (deny.toml); the exact crate list is
 in Cargo.lock of the source tree this package was built from.
EOF
  local commit
  commit=$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)
  {
    printf '%s (%s) unstable; urgency=medium\n\n' "$2" "$DEBVER"
    printf '  * Built from commit %s by packaging/build-deb.sh.\n\n' "$commit"
    printf ' -- %s  %s\n' "$MAINTAINER" "$(LC_ALL=C date -u -R -d "@$SDE")"
  } | gzip -9n > "$d/changelog.Debian.gz"
}

# <pkgdir> <pkg> <news file>: NEWS.Debian (shown by apt-listchanges before an upgrade). The file is a complete entry with
# its own version and date (the package version that introduces the change, packaging/NEWS.Debian), copied as it is: an
# entry stamped with the build's version and date would be shown again on every upgrade. The entry must not be newer
# than the package being built. Only the tessera package gets one; the other packages do not call this.
mknews() {
  local d=$1/usr/share/doc/$2 newsver
  head -n1 "$3" | grep -q "^$2 ([^)]*) " || { echo "$3 must start with a changelog header line: $2 (VERSION) ..." >&2; exit 1; }
  newsver=$(head -n1 "$3" | sed 's/^[^(]*(\([^)]*\)).*/\1/')
  dpkg --compare-versions "$newsver" le "$DEBVER" || {
    echo "$3 is for version $newsver, newer than the version being built ($DEBVER): set its first line to the version of this release" >&2
    exit 1
  }
  mkdir -p "$d"
  gzip -9n < "$3" > "$d/NEWS.Debian.gz"
}

# <pkgdir> -> DEBIAN/md5sums over every regular file outside DEBIAN
mkmd5() {
  (cd "$1" && find . -path ./DEBIAN -prune -o -type f -print | LC_ALL=C sort | sed 's|^\./||' \
     | tr '\n' '\0' | xargs -0 md5sum) > "$1/DEBIAN/md5sums"
}

# <pkgdir> <pkg> <depends> <section> <description...> ; optional env: RECOMMENDS SUGGESTS CONFLICTS
mkcontrol() {
  local d=$1 pkg=$2 deps=$3 desc=$4 size
  mkdir -p "$d/DEBIAN"
  size=$(du -sk --apparent-size --exclude=DEBIAN "$d" | cut -f1)
  {
    echo "Package: $pkg"
    echo "Version: $DEBVER"
    echo "Architecture: $ARCH"
    echo "Maintainer: $MAINTAINER"
    echo "Installed-Size: $size"
    echo "Depends: $deps"
    [ -z "${RECOMMENDS:-}" ] || echo "Recommends: $RECOMMENDS"
    [ -z "${SUGGESTS:-}" ] || echo "Suggests: $SUGGESTS"
    echo "Section: sound"
    echo "Priority: optional"
    printf '%s\n' "$desc"
  } > "$d/DEBIAN/control"
}

build_deb() { # <pkgdir> <pkg>
  local f=$OUT/${2}_${DEBVER}_${ARCH}.deb
  rm -f "$f"
  dpkg-deb --root-owner-group -Zxz -z6 --build "$1" "$f" >/dev/null
  say "built $(basename "$f")"
}

rm -rf "$STAGE"
mkdir -p "$STAGE" "$OUT"
rm -f "$OUT"/*.deb "$OUT/SHA256SUMS"

# ======================================================== package: tessera
P=$STAGE/tessera
install -d "$P/usr/bin" "$P/usr/lib/systemd/system" "$P/usr/lib/sysusers.d" \
  "$P/usr/lib/tmpfiles.d" "$P/usr/lib/sysctl.d" "$P/etc/tessera/config.d" "$P/usr/share/doc/tessera/examples" \
  "$P/usr/share/man/man1" "$P/usr/share/man/man8"
install -m 0755 "$REL/tesserad" "$REL/tesseractl" "$P/usr/bin/"
install -m 0644 dist/systemd/tesserad.service "$P/usr/lib/systemd/system/tesserad.service"
install -m 0644 crates/tessera-network/dist/tesseraaudio.slice "$P/usr/lib/systemd/system/tesseraaudio.slice"
install -m 0644 dist/sysusers.d/tessera.conf "$P/usr/lib/sysusers.d/tessera.conf"
install -m 0644 dist/tmpfiles.d/tessera.conf "$P/usr/lib/tmpfiles.d/tessera.conf"
# Socket buffer ceilings for the media sockets (the daemon warns when the kernel caps its request).
install -m 0644 dist/sysctl.d/tessera.conf "$P/usr/lib/sysctl.d/60-tessera.conf"
install -m 0644 dist/etc/tessera/config.toml "$P/etc/tessera/config.toml"
install -m 0644 dist/etc/tessera/config.toml "$P/usr/share/doc/tessera/examples/config.toml"
install -m 0644 docs/integration-contract.json "$P/usr/share/doc/tessera/integration-contract.json"
for doc in user-guide rt-tuning troubleshooting configuration network-vlans; do
  [ -f "docs/$doc.md" ] && install -m 0644 "docs/$doc.md" "$P/usr/share/doc/tessera/$doc.md"
done

if [ "$NATIVE" = 1 ]; then
  d=$P/usr/share
  install -d "$d/bash-completion/completions" "$d/zsh/vendor-completions" "$d/fish/vendor_completions.d"
  "$REL/tesseractl" completions bash > "$d/bash-completion/completions/tesseractl"
  "$REL/tesseractl" completions zsh  > "$d/zsh/vendor-completions/_tesseractl"
  "$REL/tesseractl" completions fish > "$d/fish/vendor_completions.d/tesseractl.fish"
  subs=$("$REL/tesseractl" --help | awk '/^Commands:/{f=1;next} /^$/{f=0} f && $1!="help"{print $1}')
  # shellcheck disable=SC2086
  mkman tesseractl 1 "$REL/tesseractl" "control the tessera daemon" "$P/usr/share/man/man1/tesseractl.1.gz" $subs
  mkman tesserad 8 "$REL/tesserad" "tessera audio-over-IP daemon" "$P/usr/share/man/man8/tesserad.8.gz"
else
  say "cross build: completions and man pages need a runnable binary; skipped"
fi

mkcopyright "$P" tessera
mknews "$P" tessera packaging/NEWS.Debian
# conffiles: only the main configuration file (config.d/ is the operator's).
mkdir -p "$P/DEBIAN"
echo /etc/tessera/config.toml > "$P/DEBIAN/conffiles"

cat > "$P/DEBIAN/postinst" <<'EOF'
#!/bin/sh
# Create the tessera user and the tessera/tessera-admin groups, the
# directories, and enable (not start) the service. Starting is left to the
# administrator: the daemon owns audio devices on a production network.
set -e
case "$1" in
configure)
    systemd-sysusers tessera.conf
    systemd-tmpfiles --create tessera.conf || true
    if [ -d /run/systemd/system ]; then
        systemd-sysctl 60-tessera.conf >/dev/null 2>&1 || true
    fi
    if [ -x /usr/bin/deb-systemd-helper ]; then
        deb-systemd-helper unmask tesserad.service >/dev/null || true
        if deb-systemd-helper --quiet was-enabled tesserad.service; then
            deb-systemd-helper enable tesserad.service >/dev/null || true
        else
            deb-systemd-helper update-state tesserad.service >/dev/null || true
        fi
    fi
    if [ -d /run/systemd/system ]; then
        systemctl daemon-reload || true
        if [ -n "$2" ] && systemctl --quiet is-active tesserad.service 2>/dev/null; then
            echo "tessera: tesserad is running the old version; restart it when convenient:" >&2
            echo "         systemctl restart tesserad   (a restart drops active audio flows)" >&2
        fi
    fi
    ;;
esac
exit 0
EOF
cat > "$P/DEBIAN/prerm" <<'EOF'
#!/bin/sh
# Stop the daemon on removal. Not on upgrade: the running daemon keeps its
# audio flows; restart it deliberately after the upgrade.
set -e
case "$1" in
remove|deconfigure)
    if [ -d /run/systemd/system ]; then
        systemctl stop tesserad.service || true
    fi
    ;;
esac
exit 0
EOF
cat > "$P/DEBIAN/postrm" <<'EOF'
#!/bin/sh
# remove: configuration (/etc/tessera) and state (/var/lib/tessera) stay.
# purge: both are deleted. The tessera user and the tessera and tessera-admin
# groups are never deleted (systemd-sysusers semantics: files owned by them
# may exist elsewhere); remove them by hand with userdel/groupdel if wanted.
set -e
case "$1" in
remove|purge)
    rmdir /run/tessera 2>/dev/null || true
    ;;
esac
case "$1" in
purge)
    if [ -x /usr/bin/deb-systemd-helper ]; then
        deb-systemd-helper purge tesserad.service >/dev/null || true
        deb-systemd-helper unmask tesserad.service >/dev/null || true
    fi
    rm -rf /etc/tessera /var/lib/tessera
    ;;
esac
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
fi
exit 0
EOF
chmod 0755 "$P/DEBIAN/postinst" "$P/DEBIAN/prerm" "$P/DEBIAN/postrm"

normalise "$P"
SHL=$(shlibdeps "$P/usr/bin/tesserad" "$P/usr/bin/tesseractl")
RECOMMENDS=avahi-daemon SUGGESTS='tessera-pw, tessera-alsa, rt-tests, linuxptp' \
mkcontrol "$P" tessera "$SHL, systemd (>= ${SYSTEMD_MIN:-253}), init-system-helpers (>= 1.62)" \
"Description: tessera audio-over-IP daemon and control tool
 tesserad is a Linux daemon that makes this machine an audio-over-IP device
 on the local network: named channels, subscriptions, a PTP-synchronised
 media clock and a local control socket. tesseractl is its command line.
 .
 This package ships the systemd service (enabled at boot, but
 not started on install), the tessera and tessera-admin groups, the sample
 configuration and the documentation. Audio applications connect through
 tessera-pw (PipeWire) or tessera-alsa (ALSA).
 .
 The wire protocol support is provisional until verified in the lab."
mkmd5 "$P"
build_deb "$P" tessera

# ===================================================== package: tessera-pw
if [ "$HAVE_PW" = 1 ]; then
  P=$STAGE/tessera-pw
  install -d "$P/usr/bin" "$P/usr/lib/systemd/user" "$P/usr/share/man/man1"
  install -m 0755 "$REL/tessera-pw" "$P/usr/bin/"
  install -m 0644 crates/tessera-backend-pipewire/dist/tessera-pw.service "$P/usr/lib/systemd/user/tessera-pw.service"
  if [ "$NATIVE" = 1 ]; then
    mkman tessera-pw 1 "$REL/tessera-pw" "PipeWire client of tesserad" "$P/usr/share/man/man1/tessera-pw.1.gz"
  fi
  mkcopyright "$P" tessera-pw
  normalise "$P"
  SHL=$(shlibdeps "$P/usr/bin/tessera-pw")
  RECOMMENDS=wireplumber \
  mkcontrol "$P" tessera-pw "$SHL, tessera (= $DEBVER), pipewire" \
"Description: PipeWire client for the tessera daemon
 tessera-pw exposes the channels of a tessera device as PipeWire nodes (one
 source for received audio, one sink for transmitted audio), driven by the
 network media clock.
 .
 It ships a systemd user unit. It is not enabled automatically: as the user
 that runs PipeWire, enable it with
 'systemctl --user enable --now tessera-pw.service' and make that user a
 member of the tessera group."
  mkmd5 "$P"
  build_deb "$P" tessera-pw
else
  say "note: tessera-pw not packaged (SKIP_PW=1 or no binary at $REL/tessera-pw)"
fi

# ==================================================== package: tessera-alsa
if [ "$HAVE_ALSA" = 1 ]; then
  P=$STAGE/tessera-alsa
  install -d "$P/usr/lib/$MULTIARCH/alsa-lib" "$P/usr/share/alsa/alsa.conf.d" "$P/etc/alsa/conf.d"
  install -m 0644 "$ALSA_SO" "$P/usr/lib/$MULTIARCH/alsa-lib/libasound_module_pcm_tessera.so"
  strip --strip-unneeded "$P/usr/lib/$MULTIARCH/alsa-lib/libasound_module_pcm_tessera.so" 2>/dev/null || true
  SNIP=$(ls crates/tessera-backend-alsa/dist/alsa.conf.d/*.conf 2>/dev/null | head -n1 || true)
  if [ -n "$SNIP" ]; then
    install -m 0644 "$SNIP" "$P/usr/share/alsa/alsa.conf.d/60-tessera.conf"
  else
    say "note: no crates/tessera-backend-alsa/dist/alsa.conf.d/*.conf; using the minimal snippet"
    printf '# tessera ALSA plugin: provides the PCM "tessera".\npcm.tessera {\n    type tessera\n}\n' \
      > "$P/usr/share/alsa/alsa.conf.d/60-tessera.conf"
    chmod 0644 "$P/usr/share/alsa/alsa.conf.d/60-tessera.conf"
  fi
  ln -s ../../../usr/share/alsa/alsa.conf.d/60-tessera.conf "$P/etc/alsa/conf.d/60-tessera.conf"
  mkcopyright "$P" tessera-alsa
  normalise "$P"
  SHL=$(shlibdeps "$P/usr/lib/$MULTIARCH/alsa-lib/libasound_module_pcm_tessera.so")
  mkcontrol "$P" tessera-alsa "$SHL, tessera (= $DEBVER)" \
"Description: ALSA PCM plugin for the tessera daemon
 Provides the ALSA PCM device 'tessera', backed by the channels of a tessera
 device, for applications that talk to ALSA directly. The user must be a
 member of the tessera group."
  mkmd5 "$P"
  build_deb "$P" tessera-alsa
else
  say "note: tessera-alsa not packaged: libasound_module_pcm_tessera.so not found under $REL"
fi

# ===================================================== package: tessera-web
P=$STAGE/tessera-web
install -d "$P/usr/bin" "$P/usr/lib/systemd/system" "$P/usr/lib/sysusers.d" "$P/etc/tessera" \
  "$P/usr/share/doc/tessera-web/examples" "$P/usr/share/man/man8"
install -m 0755 "$REL/tessera-web" "$P/usr/bin/"
install -m 0755 "$REL/tessera-network" "$P/usr/bin/"
install -m 0644 crates/tessera-network/dist/tessera-network.service crates/tessera-network/dist/tessera-network-recovery.service crates/tessera-network/dist/tessera-network-recovery.timer "$P/usr/lib/systemd/system/"
install -d "$P/usr/lib/systemd/system/tesserad.service.d"
install -m 0644 crates/tessera-network/dist/network-recovery.conf "$P/usr/lib/systemd/system/tesserad.service.d/standalone-network.conf"
install -m 0644 crates/tessera-web/dist/tessera-web.service "$P/usr/lib/systemd/system/tessera-web.service"
install -m 0644 "crates/tessera-web/dist/tessera-web-unitfile@.service" "$P/usr/lib/systemd/system/tessera-web-unitfile@.service"
install -m 0644 crates/tessera-web/dist/tessera-web.sysusers.conf "$P/usr/lib/sysusers.d/tessera-web.conf"
install -m 0644 crates/tessera-web/dist/web.toml "$P/etc/tessera/web.toml"
install -m 0644 crates/tessera-web/dist/web.toml "$P/usr/share/doc/tessera-web/examples/web.toml"
install -m 0644 crates/tessera-web/dist/appliance-paths.conf "$P/usr/share/doc/tessera-web/examples/appliance-paths.conf"
if [ "$NATIVE" = 1 ]; then
  mkman tessera-web 8 "$REL/tessera-web" "web portal of the tessera daemon" "$P/usr/share/man/man8/tessera-web.8.gz"
fi
mkcopyright "$P" tessera-web
mkdir -p "$P/DEBIAN"
echo /etc/tessera/web.toml > "$P/DEBIAN/conffiles"
cat > "$P/DEBIAN/postinst" <<'EOF'
#!/bin/sh
# Create the tessera-web user (member of group tessera) and enable the
# service. It is not started: it needs an operator password first.
set -e
case "$1" in
configure)
    systemd-sysusers tessera.conf tessera-web.conf
    if [ -x /usr/bin/deb-systemd-helper ]; then
        deb-systemd-helper enable tessera-network.service tessera-network-recovery.timer >/dev/null || true
    fi
    if [ -x /usr/bin/deb-systemd-helper ]; then
        deb-systemd-helper unmask tessera-web.service >/dev/null || true
        if deb-systemd-helper --quiet was-enabled tessera-web.service; then
            deb-systemd-helper enable tessera-web.service >/dev/null || true
        else
            deb-systemd-helper update-state tessera-web.service >/dev/null || true
        fi
    fi
    if [ -d /run/systemd/system ]; then
        systemctl daemon-reload || true
        if [ -z "$2" ]; then
            systemctl start tessera-network-recovery.timer || true
            echo "tessera-web: set a password, then start the portal:" >&2
            echo "             sudo tessera-web set-password && sudo systemctl enable --now tessera-web" >&2
        elif systemctl --quiet is-active tessera-web.service 2>/dev/null; then
            systemctl restart tessera-web.service || true
        fi
    fi
    ;;
esac
exit 0
EOF
cat > "$P/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
case "$1" in
remove|deconfigure)
    if [ -d /run/systemd/system ]; then
        /usr/bin/tessera-network boot-recover
        systemctl stop tessera-web.service || true
        systemctl stop tessera-network-recovery.timer tessera-network.service || true
    fi
    ;;
esac
exit 0
EOF
cat > "$P/DEBIAN/postrm" <<'EOF'
#!/bin/sh
# purge: the password file goes too. The tessera-web user is never deleted
# (systemd-sysusers semantics).
set -e
case "$1" in
purge)
    if [ -x /usr/bin/deb-systemd-helper ]; then
        deb-systemd-helper purge tessera-web.service >/dev/null || true
        deb-systemd-helper purge tessera-network.service tessera-network-recovery.timer >/dev/null || true
        deb-systemd-helper unmask tessera-web.service >/dev/null || true
    fi
    rm -f /etc/tessera/web.toml /etc/tessera/web-password
    ;;
esac
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
fi
exit 0
EOF
chmod 0755 "$P/DEBIAN/postinst" "$P/DEBIAN/prerm" "$P/DEBIAN/postrm"
normalise "$P"
SHL=$(shlibdeps "$P/usr/bin/tessera-web" "$P/usr/bin/tessera-network")
mkcontrol "$P" tessera-web "$SHL, tessera (= $DEBVER), systemd (>= ${SYSTEMD_MIN:-253}), init-system-helpers (>= 1.62), iproute2, nftables, iputils-arping, coreutils" \
"Description: web portal for the tessera daemon
 tessera-web serves pages and a JSON API for a tessera device: status, the
 signal path, the network interface, device settings, subscriptions, the
 clock, metrics, events and the configuration in effect. It talks to the
 daemon over its local control socket. Standalone VLAN configuration uses a
 separate privileged Rust helper with device-side recovery.
 .
 Every page needs the operator password (set it with
 'tessera-web set-password'). By default it listens on 127.0.0.1:8090 only;
 see /etc/tessera/web.toml."
mkmd5 "$P"
build_deb "$P" tessera-web

(cd "$OUT" && sha256sum ./*.deb | sed 's| \./| |' > SHA256SUMS)
say "done:"
ls -l "$OUT"/*.deb >&2
