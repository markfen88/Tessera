# Bundle manifest v1

`manifest.json` is the feed/client contract. Required fields: `name` (tessera),
`version` (Debian version), `arch` (amd64 or arm64), `built` (UTC ISO 8601),
`commit` (full source SHA), `os` (`id`: debian, `codename`: trixie, `version_id`: "13",
`systemd_min`: 253), `apt_depends` (array of Debian dependency expressions),
`debs` (array of objects with bundle-relative `file`, `package`, `sha256`),
`dsp` (boolean, additive: whether `debs` includes the `tessera-dsp` package, which the
speaker profile installs by default; absent in manifests of earlier bundles, meaning false),
`profiles` (speaker, ndimon), `min_ndimon` (1.3.0).
Experimental native Ubuntu arm64 bundles use `os.id`: ubuntu, `codename`:
noble and `version_id`: "24.04". `version_id` is additive build provenance;
older Debian manifests may omit it. Both version IDs are JSON strings.
`apt_depends` has one entry per package; repeated minimum versions retain the
highest Debian version, compared with `dpkg --compare-versions`.

`manifest.env` carries the same metadata as BUNDLE_NAME, BUNDLE_VERSION,
BUNDLE_ARCH, BUNDLE_BUILT, BUNDLE_COMMIT, BUNDLE_OS_ID, BUNDLE_OS_CODENAME,
BUNDLE_OS_VERSION_ID, BUNDLE_SYSTEMD_MIN, BUNDLE_MIN_NDIMON, BUNDLE_PROFILES, BUNDLE_DSP (1 or 0, agreeing with
`dsp`; missing means 0), BUNDLE_DEBS (space
separated relative paths), BUNDLE_DEB_PACKAGES and BUNDLE_DEB_SHA256 (arrays in
the same deb order), BUNDLE_APT_DEPENDS (legacy array data preserving constraints).
The deb hashes are also in SHA256SUMS. Both formats are parsed as data and
cross-checked by the Rust bundle verifier; no assignment is evaluated.
Before using either manifest, the verifier authenticates SHA256SUMS.minisig
with trusted minisign public keys and checks every payload hash. Its authenticated
comment binds product `tessera`, this manifest's version and `SHA256SUMS`.
The compiled key list starts empty; provision public trust through the protected
root-owned override described in [README.md](README.md). Unsigned development
installation requires explicit `--allow-unsigned`; integrity checks still apply.
