# Bundle manifest v1

`manifest.json` is the feed/client contract. Required fields: `name` (tessera),
`version` (Debian version), `arch` (amd64 or arm64), `built` (UTC ISO 8601),
`commit` (full source SHA), `os` (`id`: debian, `codename`: trixie, `version_id`: "13",
`systemd_min`: 253), `apt_depends` (array of Debian dependency expressions),
`debs` (array of objects with bundle-relative `file`, `package`, `sha256`),
`profiles` (speaker, ndimon), `min_ndimon` (1.3.0).
Experimental native Ubuntu arm64 bundles use `os.id`: ubuntu, `codename`:
noble and `version_id`: "24.04". `version_id` is additive build provenance;
older Debian manifests may omit it. Both version IDs are JSON strings.
`apt_depends` has one entry per package; repeated minimum versions retain the
highest Debian version, compared with `dpkg --compare-versions`.

`manifest.env` carries the same metadata as BUNDLE_NAME, BUNDLE_VERSION,
BUNDLE_ARCH, BUNDLE_BUILT, BUNDLE_COMMIT, BUNDLE_OS_ID, BUNDLE_OS_CODENAME,
BUNDLE_OS_VERSION_ID, BUNDLE_SYSTEMD_MIN, BUNDLE_MIN_NDIMON, BUNDLE_PROFILES, BUNDLE_DEBS (space
separated relative paths), BUNDLE_DEB_PACKAGES and BUNDLE_DEB_SHA256 (arrays in
the same deb order), BUNDLE_APT_DEPENDS (bash array preserving constraints).
The deb hashes are also in SHA256SUMS. These files are trusted release code;
checksums detect corruption, not authenticity. Obtain bundles from a trusted source.
