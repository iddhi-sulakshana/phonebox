# Releasing PhoneBox

The release version is `agent-rs/Cargo.toml`'s (`[workspace.package] version`): `x.y.z`, or `x.y.z-alpha.N` for a
pre-release. Since 1.0.0 the Rust agent is PhoneBox. A final version needs the release workflow's `final` input (the
owner decides when). A pre-release is published as a GitHub **pre-release** that never becomes "latest", so a stable
install, setup without `--pre` and a stable agent's updater never take it. The C# project (`agent/`) stays at
`0.9.23` as the parity oracle until it is removed, and is not released any more.

## Bump the version

One commit per release:

1. `agent-rs/Cargo.toml`: `version = "x.y.z"` (or `"x.y.z-alpha.N"`), then refresh `agent-rs/Cargo.lock` (any
   `cargo build` in `agent-rs` does it; the release builds with `--locked`, so a stale lock fails).
2. `android/app/build.gradle.kts`: the same `versionName`, and `versionCode` one higher than the last release (the
   fork started at 100 for `1.0.0-alpha.1`; 1.0.0 is 102).
3. `CHANGELOG.md`: a dated section, `## x.y.z — YYYY-MM-DD`. Its text becomes the release notes.
4. After a dependency change, regenerate `agent-rs/THIRD-PARTY-NOTICES.txt` with `agent-rs/scripts/notices.sh`
   (cargo-about on the test server). The notices ship in every agent zip.

Then check, on the test server (`ssh` after `agent-rs/scripts/_sync.sh`'s `sync_tree`, run from bash):
`python3 tools/check-release.py --final` (without `--final` it refuses a final version), `python3 tools/check-notices.py`, `python3 tools/test-release-metadata.py`, and
`agent-rs/scripts/remote-check.sh` for the Rust tests.

## Release from the Mac (GitHub Actions off)

Since 2026-11-23 GitHub Actions is turned off on the fork (it used up the Actions minutes), and releases are built on
the maintainer's Mac (Apple silicon) with `tools/release-local.sh`. After the version bump is committed and pushed to
the fork's `main`:

```bash
tools/release-local.sh            # builds dist-local/ and dist-local.notes.md, publishes nothing
tools/release-local.sh --publish  # the same, then gh release create v<version> (latest) from main's HEAD
```

It builds the macOS agents (`osx-arm64`, and `osx-x64` cross-compiled), signs both with PhoneBox's own certificate in
a throwaway keychain (the same designated requirement as every release, so macOS keeps the Accessibility permission),
builds the release APK signed with the fork's key (its certificate fingerprint is checked), and writes
`SHA256SUMS-<version>.txt` and the notes from `CHANGELOG.md`. The secrets come from `/Volumes/storage/Secrets/phonebox`
(`PHONEBOX_SECRETS`) and are never printed; the Android build uses a JDK 17-21 (`PHONEBOX_JAVA_HOME`, default
`/Volumes/storage/Runtimes/jdk21`), whatever `JAVA_HOME` the shell has.

What such a release does not have: the Linux and Windows agents, the Scoop and winget manifest update, and a GitHub
build attestation (only Actions can make one). Installed agents update themselves from it as usual; `setup.sh` and
`setup.ps1` refuse its downloads while a signed-in GitHub CLI cannot verify the attestation. Nothing runs the agent on
the Mac (it would start a server): the tests and the checks below still run on the test server
(`agent-rs/scripts/remote-check.sh`, `remote-update-check.sh`) before the bump is pushed.

To turn Actions back on: `gh api -X PUT repos/iddhi-sulakshana/phonebox/actions/permissions -F enabled=true`, then
the workflow below applies again.

## Run the release

Push the branch and wait for the **rust** and **build** workflows to pass, then dispatch **release** on that branch:

```
gh workflow run release.yml --repo iddhi-sulakshana/phonebox --ref <branch> -f draft=true
```

Review the draft's assets, delete it, and dispatch again with `-f draft=false`: a draft's downloads are not public,
so setup and the updater cannot fetch them. For a final version add `-f final=true` (only with the owner's go-ahead)
and publish from `main` (`--ref main`), so the package manifests follow.

The workflow:

- runs the C# CI (`build.yml`, the parity oracle) and the Rust CI (`rust.yml`: tests on five runners, parity, the
  notices and version checks, and the same packaging as the release);
- reads the version, checks it with `tools/check-release.py` (and `--final` when asked), and refuses a tag that
  exists already;
- builds and signs the APK;
- builds `PhoneBoxAgent` with `cargo build --release --locked` on Windows x64, Linux x64 and ARM64 (Ubuntu 22.04, so
  glibc 2.35 is enough), macOS ARM64 and Intel; signs the macOS agent with PhoneBox's own certificate; checks the
  Windows exe (no console, no Visual C++ runtime, `ProductVersion` = the version) and the Linux glibc floor;
  smoke-tests each agent on its own architecture (`tools/smoke-agent.py`); zips the executable and the notices at the
  zip root (`tools/zip-agent.py`);
- attests the zips and the APK, writes `SHA256SUMS-<v>.txt`, and creates release `v<v>` with the CHANGELOG section as
  notes, as a pre-release (`--prerelease --latest=false`) when the version has a `-`.

The asset names are the C# releases', byte for byte, because setup and the updater look for them:
`PhoneBoxAgent-<v>-<rid>.zip` (`win-x64`, `linux-x64`, `linux-arm64`, `osx-x64`, `osx-arm64`), `PhoneBox-<v>.apk`,
`PhoneBox-<v>.intoto.jsonl` and `SHA256SUMS-<v>.txt`.

Only a published final release dispatched from `main` updates the Scoop bucket (`bucket/phonebox.json`) and the
winget manifests (`packaging/winget/manifests/i/iddhi-sulakshana/PhoneBox/<v>/`, identifier
`iddhi-sulakshana.PhoneBox`), as a commit pushed to `main`; pre-releases and other branches never do. Submitting the
winget manifests to microsoft/winget-pkgs is manual: `wingetcreate submit <folder>` (check them first with
`winget validate --manifest <folder>`).

## Secrets

The release stops before publishing anything without these repository secrets:

- `PHONEBOX_KEYSTORE_BASE64` (base64 of the keystore), `PHONEBOX_STORE_PASSWORD`, `PHONEBOX_KEY_ALIAS` and
  `PHONEBOX_KEY_PASSWORD`: the APK's signing key. Keep it: Android refuses an update signed with another key, so a
  phone with an app signed by an older key has to uninstall it once.
- `MACOS_SIGNING_P12_BASE64` and `MACOS_SIGNING_P12_PASSWORD`: the macOS certificate made once with
  `tools/make-macos-signing-cert.sh`. Every release then has the same designated requirement,
  `identifier "net.oxog.phonebox.agent"` plus that certificate, so macOS keeps the Accessibility permission across
  updates. A new certificate or identifier makes every Mac grant it again. No hardened runtime.

Keep the key, the certificate and their passwords out of the repository. The Windows agent is not code-signed.

## After publishing

Check that the tag points at the intended commit, that every asset matches `SHA256SUMS-<v>.txt`, and that
`gh attestation verify <asset> --repo iddhi-sulakshana/phonebox` passes for the APK and the agent zips. Extract a
downloaded zip and run `python3 tools/smoke-agent.py <executable> --version <v>` on the machines at hand. Then
dispatch **lifecycle** (`-f from=<the previous release> -f to=<v>`: an install of the previous release must update
itself to this one) and **setup** (the installers against the new latest release). Report CI results,
downloaded-binary checks and checks on a real phone separately.
