#!/usr/bin/env bash
# Usage: tools/release-local.sh [--publish] [--draft]
#
# A release built on the maintainer's Mac (Apple silicon) instead of GitHub Actions: the macOS agents (osx-arm64 and
# osx-x64) signed with PhoneBox's own certificate, and the Android app signed with the fork's release key, zipped and
# checksummed as release.yml does. The Linux and Windows agents are not built, and there is no build provenance
# attestation (only Actions can make one), so setup.sh with a signed-in GitHub CLI refuses these downloads; installed
# agents update themselves as usual.
#
# Without --publish it stops after writing dist-local/ (the release files) and dist-local.notes.md. With --publish it creates the GitHub
# release v<version> from main's HEAD, which must be clean and pushed to the fork; --draft makes it a draft.
#
# Secrets (never printed): $PHONEBOX_SECRETS (default /Volumes/storage/Secrets/phonebox) holds
#   macos/phonebox-macos-signing.p12 + .password   (tools/make-macos-signing-cert.sh)
#   android/phonebox-release.p12 + .password, android/certificate-sha256.txt
# Toolchains: Rust from rust-toolchain.toml (CARGO_HOME/RUSTUP_HOME), a JDK 17-21 ($PHONEBOX_JAVA_HOME), the Android SDK
# ($ANDROID_HOME). Nothing runs the agent here: it would start a server.
set -euo pipefail

repo=iddhi-sulakshana/phonebox
publish=0
draft=0
for a in "$@"; do
  case "$a" in
    --publish) publish=1 ;;
    --draft) draft=1 ;;
    -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
secrets="${PHONEBOX_SECRETS:-/Volumes/storage/Secrets/phonebox}"
export CARGO_HOME="${CARGO_HOME:-/Volumes/storage/Runtimes/Rust/cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-/Volumes/storage/Runtimes/Rust/rustup}"
export PATH="$CARGO_HOME/bin:$PATH"
# the Android build needs a JDK 17-21 whatever JAVA_HOME the shell has (a newer one fails with just its version)
android_java="${PHONEBOX_JAVA_HOME:-/Volumes/storage/Runtimes/jdk21/Contents/Home}"
export ANDROID_HOME="${ANDROID_HOME:-/Volumes/storage/Emulators}"
export GRADLE_USER_HOME="${GRADLE_USER_HOME:-/Volumes/storage/Runtimes/gradle-home}"

say() { printf '\n== %s\n' "$*"; }
die() { printf 'release-local: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)/$(uname -m)" = Darwin/arm64 ] || die "this builds on an Apple silicon Mac"
for f in macos/phonebox-macos-signing.p12 macos/phonebox-macos-signing.password android/phonebox-release.p12 \
         android/phonebox-release.password android/certificate-sha256.txt; do
  [ -s "$secrets/$f" ] || die "missing $secrets/$f"
done
[ -x "$android_java/bin/java" ] || die "no JDK at $android_java (set PHONEBOX_JAVA_HOME to a JDK 17-21)"
java_major="$("$android_java/bin/java" -version 2>&1 | sed -n 's/.*version "\([0-9]*\).*/\1/p' | head -n1)"
[ -n "$java_major" ] && [ "$java_major" -ge 17 ] && [ "$java_major" -le 21 ] \
  || die "the JDK at $android_java is ${java_major:-unknown}; the Android build needs 17-21 (PHONEBOX_JAVA_HOME)"
[ -d "$ANDROID_HOME/platforms" ] || die "no Android SDK at ANDROID_HOME=$ANDROID_HOME"

v="$(sed -n 's/^version = "\(.*\)"$/\1/p' agent-rs/Cargo.toml | head -n1)"
[ -n "$v" ] || die "no version in agent-rs/Cargo.toml"
say "PhoneBox $v"
python3 -I tools/check-release.py --final
sha="$(git rev-parse HEAD)"
if [ $publish = 1 ]; then
  [ -z "$(git status --porcelain)" ] || die "the tree has uncommitted changes"
  git fetch -q fork main
  [ "$sha" = "$(git rev-parse fork/main)" ] || die "HEAD is not the fork's main: push it first"
  if gh release view "v$v" -R "$repo" >/dev/null 2>&1; then die "v$v already exists on $repo"; fi
fi

dist="$root/dist-local"
rm -rf "$dist"
mkdir -p "$dist"
work="$(mktemp -d)"
kc="$work/phonebox-signing.keychain-db"
# the user's keychain search list, put back as it was when the script ends
keychains=()
while IFS= read -r line; do
  line="${line#"${line%%[![:space:]]*}"}"
  line="${line#\"}"
  line="${line%\"}"
  [ -n "$line" ] && keychains+=("$line")
done < <(security list-keychains -d user)
cleanup() {
  [ ${#keychains[@]} -gt 0 ] && security list-keychains -d user -s "${keychains[@]}" 2>/dev/null || true
  security delete-keychain "$kc" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

say "agents (cargo build --release)"
toolchain="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' agent-rs/rust-toolchain.toml)"
rustup target add x86_64-apple-darwin --toolchain "$toolchain" >/dev/null
(cd agent-rs && cargo build --release --locked -p phonebox-agent)
(cd agent-rs && cargo build --release --locked -p phonebox-agent --target x86_64-apple-darwin)

say "signing with PhoneBox's certificate (a keychain of its own, removed afterwards)"
kcpass="$(openssl rand -hex 24)"
security create-keychain -p "$kcpass" "$kc"
security set-keychain-settings -lut 900 "$kc"
security unlock-keychain -p "$kcpass" "$kc"
security import "$secrets/macos/phonebox-macos-signing.p12" -k "$kc" \
  -P "$(cat "$secrets/macos/phonebox-macos-signing.password")" -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$kcpass" "$kc" >/dev/null
# codesign finds identities only in keychains on the search list
security list-keychains -d user -s "$kc" "${keychains[@]}"
identity="$(security find-certificate -a -Z "$kc" | awk '/SHA-1 hash:/ { print $3; exit }')"
[ -n "$identity" ] || die "no certificate in the macOS PKCS#12"

for rid in osx-arm64 osx-x64; do
  case "$rid" in
    osx-arm64) bin=agent-rs/target/release/PhoneBoxAgent; arch=arm64 ;;
    osx-x64) bin=agent-rs/target/x86_64-apple-darwin/release/PhoneBoxAgent; arch=x86_64 ;;
  esac
  out="$work/$rid"
  mkdir -p "$out"
  cp "$bin" agent-rs/THIRD-PARTY-NOTICES.txt "$out/"
  # same identifier and certificate as every release: macOS keeps the Accessibility permission across updates
  codesign --force --sign "$identity" --keychain "$kc" --identifier net.oxog.phonebox.agent --timestamp=none \
    "$out/PhoneBoxAgent"
  codesign --verify --strict "$out/PhoneBoxAgent"
  requirement="$(codesign -d -r- "$out/PhoneBoxAgent" 2>&1 | sed -n 's/^designated => //p')"
  case "$requirement" in
    *'identifier "net.oxog.phonebox.agent"'*'certificate leaf = H"'*) ;;
    *) die "unexpected designated requirement for $rid: $requirement" ;;
  esac
  [ "$(lipo -archs "$out/PhoneBoxAgent")" = "$arch" ] || die "$rid is not $arch"
  grep -aq "$v" "$out/PhoneBoxAgent" || die "$rid does not carry version $v"
  python3 tools/zip-agent.py "$out" "$dist/PhoneBoxAgent-$v-$rid.zip"
  echo "   $rid: signed, $arch, $(du -h "$dist/PhoneBoxAgent-$v-$rid.zip" | cut -f1) zip"
done

say "the Android app (gradlew assembleRelease)"
(
  cd android
  export JAVA_HOME="$android_java"
  export PHONEBOX_STORE_FILE="$secrets/android/phonebox-release.p12"
  PHONEBOX_STORE_PASSWORD="$(cat "$secrets/android/phonebox-release.password")"
  export PHONEBOX_STORE_PASSWORD PHONEBOX_KEY_PASSWORD="$PHONEBOX_STORE_PASSWORD" PHONEBOX_KEY_ALIAS=phonebox
  ./gradlew --no-daemon --console=plain -q assembleRelease
)
apk=android/app/build/outputs/apk/release/app-release.apk
[ -s "$apk" ] || die "no APK at $apk"
apksigner="$(ls -d "$ANDROID_HOME"/build-tools/*/apksigner | sort -V | tail -n1)"
want="$(tr -d ' :\n' < "$secrets/android/certificate-sha256.txt" | tr 'A-F' 'a-f')"
got="$("$apksigner" verify --print-certs "$apk" | sed -n 's/.*certificate SHA-256 digest: //p' | head -n1 | tr -d ' :')"
[ -n "$got" ] && [ "$got" = "$want" ] || die "the APK is not signed with the fork's key"
cp "$apk" "$dist/PhoneBox-$v.apk"
echo "   APK: signed with the fork's key, $(du -h "$dist/PhoneBox-$v.apk" | cut -f1)"

say "checksums and notes"
(cd "$dist" && shasum -a 256 -- * > "SHA256SUMS-$v.txt" && cat "SHA256SUMS-$v.txt")
section() { awk -v v="$1" '/^## /{ if (on) exit; on = ($2 == v); next } on' CHANGELOG.md; }
section "$v" > "$work/notes.md"
[ -s "$work/notes.md" ] || section Unreleased > "$work/notes.md"
[ -s "$work/notes.md" ] || echo "PhoneBox $v" > "$work/notes.md"
cat >> "$work/notes.md" <<EOF

---
Built on the maintainer's Mac: the macOS agents (Apple silicon and Intel) and the Android app only. Linux and
Windows agents are not part of this release, and it carries no GitHub build attestation. Installed agents update
themselves as usual. [Privacy policy](https://github.com/$repo/blob/main/PRIVACY.md)
EOF
cp "$work/notes.md" "$root/dist-local.notes.md"

if [ $publish = 0 ]; then
  say "built in dist-local/ (not published; --publish creates the release)"
  exit 0
fi
say "publishing v$v"
flags=(--latest)
[ $draft = 1 ] && flags=(--draft)
gh release create "v$v" "$dist"/* -R "$repo" --title "PhoneBox $v" --notes-file "$work/notes.md" --target "$sha" "${flags[@]}"
gh release view "v$v" -R "$repo" --json url,isDraft,isPrerelease --jq '"\(.url) draft=\(.isDraft) prerelease=\(.isPrerelease)"'
