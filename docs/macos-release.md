# Preparing a macOS prerelease

The current testing release is `v0.1.0-beta.1`, Apple Silicon only, with macOS 15 as
its minimum deployment target. Rust is pinned by `rust-toolchain.toml` and Cargo
dependencies by the committed lockfile. Hosted Git dependencies require no
sibling checkouts. Pushing an alpha or beta tag automatically publishes a
downloadable prerelease after all build, signing and package checks pass.
[The beta checklist](release-gates.md) tracks the remaining acceptance work;
publishing a testing build does not mark those checks complete.

## Versioning

Set `[workspace.package].version` in Cargo.toml, refresh Cargo.lock, and add
`docs/releases/<version>.md`. The package script requires release notes and rejects
a `RELEASE_TAG` that differs from `v<version>` before building.

Supported channels are `X.Y.Z-alpha.N` and `X.Y.Z-beta.N`, with N from 1 to 99.
`scripts/macos_version.py` defines the shared bundle policy: the short version is
`X.Y.Z`; the numeric build version is `1.N` for alpha and `2.N` for beta.
For example, beta.1 is `2.1`, beta.2 is `2.2`, and both follow alpha.99 (`1.99`).
`TrashAmpVersion` retains the full version for display and package verification.
Stable/RC releases need an explicit policy and workflow change before use.

## Local package

```sh
python3 -m unittest discover -s scripts -p 'test_*.py'
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
./scripts/package-macos.sh
./scripts/verify-macos-package.sh target/dist/trash-amp-0.1.0-beta.1-macos-arm64.zip
```

Python 3.12 or newer, Xcode command-line tools, and network access are required.
Real Collector server tests need `flac` on PATH (`brew install flac`). Notice
generation downloads cargo-about 0.9.2 and verifies its pinned SHA-256 checksum.
Changing Slint's license or the Skia version requires updating the bundled
license texts; packaging stops until they match.

The ZIP and SHA-256 file appear in `target/dist`. The ZIP includes the app and
release notes. Ordinary local/PR/main builds use ad-hoc signing unless
`MACOS_SIGN_IDENTITY` is set. The `--distribution` bundle option includes license
notices; signed release packaging additionally requires notarization.

## Developer ID signing and notarization

Signing credentials belong to your Apple Developer team and can be reused across
your macOS apps. They have no dependency on another app or repository. The
Developer ID Application certificate signs the app; the notarization credentials
authenticate submissions for the team. Each app retains its own bundle ID;
Trash Amp uses `org.aberrantwolf.trash-amp`. No JIT/unsigned-memory exceptions are
granted. The current local signing identity is team `6DNZ4UX59T`.

For a local signed package, create or reuse a `notarytool` Keychain profile.
The profile name is a local credential label, shared across apps if desired.
To create one, run this with your Apple Account and enter its app-specific
password at the secure prompt:

```sh
xcrun notarytool store-credentials developer-id-notary \
  --apple-id your-apple-account@example.com --team-id 6DNZ4UX59T
```

Then run:

```sh
export MACOS_SIGN_IDENTITY='Developer ID Application: Ben Harper (6DNZ4UX59T)'
export MACOS_SIGNING_TEAM_ID=6DNZ4UX59T
export MACOS_NOTARY_PROFILE=developer-id-notary
RELEASE_TAG=v0.1.0-beta.1 ./scripts/package-macos.sh --signed
./scripts/verify-macos-package.sh --signed target/dist/trash-amp-0.1.0-beta.1-macos-arm64.zip
```

`MACOS_SIGNING_KEYCHAIN` optionally selects a non-default keychain for both
signing and notarization. A nonempty `RELEASE_TAG` always requires Developer ID
signing and notarization, even without `--signed`. Missing credentials, signature
failure, a rejected/timed-out submission, failed stapling or Gatekeeper rejection
stop packaging. The final ZIP and checksum are generated after stapling.
The signed submission ZIP, submission result and Apple's log are retained in
`target/notarization/<version>` and uploaded as the `macos-notarization` Actions
artifact. Apple's log is requested for successful submissions as well as failures.
A timeout may leave a submission processing at Apple, so inspect its ID/status
before resubmitting. The submission ZIP is a recovery artifact; the distribution
ZIP additionally contains release notes and the stapled ticket.

`scripts/staple-macos.sh APP` retries ticket-service errors 68, 69 and 75 up to
three times per stapling or validation operation, with 10-second delays, then
checks Gatekeeper. Extracted-package verification uses its `--validate-only`
mode to retry transient validation errors without modifying the extracted app.
Invalid or revoked tickets fail immediately. If ticket delivery
fails after Apple accepted the submission, the retained ZIP can be extracted and
this helper rerun on the exact signed app without rebuilding or uploading it
again. Recreate the distribution archive/checksum only after stapling succeeds.

### GitHub repository secrets

Configure these under the [Trash Amp Actions secrets](https://github.com/AberrantWolf/trash-amp/settings/secrets/actions).
These generic secret names can be used by every macOS release pipeline:

| Secret | Value |
| --- | --- |
| `MACOS_CERT_P12_BASE64` | Base64 of the exported Developer ID Application certificate **and private key** (`.p12`) |
| `MACOS_CERT_PASSWORD` | Password protecting that `.p12` |
| `MACOS_KEYCHAIN_PASSWORD` | Password for the temporary runner keychain; a new random value is fine |
| `MACOS_NOTARY_APPLE_ID` | Apple Account used for notarization |
| `MACOS_NOTARY_PASSWORD` | Its app-specific password |
| `MACOS_NOTARY_TEAM_ID` | Signing team ID, currently `6DNZ4UX59T` |

Apple credentials are team-wide; GitHub controls which repositories can use their
stored copies. Repository secrets must be populated separately in each repo.
An organization can instead grant selected repositories access to organization
secrets with these same names. GitHub returns secret names and metadata, not
their values; configure access from the original credentials. The existing
certificate can be reused without creating another certificate or an App Store
Connect app. For a locally exported `.p12`, upload it directly:

```sh
base64 -i /path/to/developer-id.p12 | gh secret set MACOS_CERT_P12_BASE64 --repo AberrantWolf/trash-amp
gh secret set MACOS_CERT_PASSWORD --repo AberrantWolf/trash-amp
```

Set the remaining secrets through the repository UI or the same interactive
`gh secret set` command. The workflow imports credentials only for tags or an
explicit signed manual run, validates that the certificate matches the notary
team, and stores notarization credentials in an isolated temporary keychain.
The keychain is temporarily added to the runner's search list so `codesign` can
resolve the identity and certificate chain. Cleanup restores the original search
list and removes the temporary keychain after success or failure. PR/main builds
receive no signing secrets.

### Apple account access failures

A successful certificate import does not establish access to Apple's notary
service. If credential validation returns HTTP 403 with “A required agreement
is missing or has expired,” the team's Account Holder must review the pending
agreement in [the Apple Developer account](https://developer.apple.com/account/).
Confirm the selected team matches `MACOS_NOTARY_TEAM_ID`; also verify that the
program membership is active. Apple documents these restrictions under
[resolving access issues](https://developer.apple.com/help/account/access/resolving-access-issues/).
After the account issue is resolved, allow time for the change to propagate and
rerun the failed Actions job. Re-exporting a successfully imported certificate
does not resolve this account-level rejection.

## GitHub build and automatic publication

`.github/workflows/macos-alpha.yml` (display name **macOS prerelease**) validates
and packages pull requests, main
pushes and manual runs. These runs upload a `macos-arm64-prerelease` Actions
artifact. Manual runs offer a `notarize` checkbox for signed candidate validation
before tagging. Alpha and beta tags require signed, notarized packages and
automatically publish a **prerelease** only after validation, packaging,
extracted-ticket/Gatekeeper checks and downloaded-artifact checksum verification
succeed. Missing secrets fail the run; tags cannot fall back to ad-hoc signing.
`scripts/publish-prerelease.sh` uploads the ZIP and checksum before publication.
It finishes an interrupted draft automatically; upload failures leave it
unpublished. Rerunning an already published tag leaves its notes and assets
unchanged. Testing prereleases are not marked as the latest stable release.

The workflow retains its original filename so the existing Actions registration
can dispatch a signed candidate from a branch before the changes are merged.

Once the reviewed version/lockfile/notes changes are on main and Actions passes,
push the new version's tag. For example, after updating the workspace version,
lockfile and notes to `0.1.0-beta.2`:

```sh
git tag -a v0.1.0-beta.2 -m 'Trash Amp 0.1.0 beta 2'
git push origin v0.1.0-beta.2
```

No separate Publish click or local `gh release edit` command is required.
Download the published ZIP and checksum from Releases and test on a separate Mac.
Complete and record the [beta checklist](release-gates.md), including
upgrade/Keychain checks with the final signing identity. Confirm Finder launch,
version and Slint attribution, folders/restart, local audio, Collector
enrollment/streaming/downloads/offline playback and media keys. Record known
limits in the version's release notes before tagging; remaining native checks
stay open until there is evidence for the tested package.
