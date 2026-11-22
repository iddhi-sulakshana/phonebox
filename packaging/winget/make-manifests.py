#!/usr/bin/env python3
"""Writes the winget manifests for a release:  python packaging/winget/make-manifests.py 0.6.3 <sha256 of the win-x64 zip>

They land in packaging/winget/manifests/i/iddhi-sulakshana/PhoneBox/<version>/ (the layout of microsoft/winget-pkgs; this
fork's identifier since 1.0.0, upstream's is ersinkoc.PhoneBox). Check them with `winget validate --manifest <folder>`
and submit them with `wingetcreate submit <folder>` (or a pull request to microsoft/winget-pkgs): nothing is submitted
automatically. A published final release from main runs this script and commits the result to main.
"""
import sys
from pathlib import Path

if len(sys.argv) != 3:
    sys.exit(__doc__)
version, sha = sys.argv[1], sys.argv[2].upper()
owner, pkg = 'iddhi-sulakshana', 'iddhi-sulakshana.PhoneBox'
repo = f'https://github.com/{owner}/phonebox'
out = Path(__file__).resolve().parent / 'manifests' / owner[0] / owner / 'PhoneBox' / version
out.mkdir(parents=True, exist_ok=True)
schema = '1.6.0'
header = '# yaml-language-server: $schema=https://aka.ms/winget-manifest.{kind}.' + schema + '.schema.json\n'

(out / f'{pkg}.yaml').write_text(header.format(kind='version') + f"""
PackageIdentifier: {pkg}
PackageVersion: {version}
DefaultLocale: en-US
ManifestType: version
ManifestVersion: {schema}
""".lstrip('\n'), encoding='utf-8')

(out / f'{pkg}.installer.yaml').write_text(header.format(kind='installer') + f"""
PackageIdentifier: {pkg}
PackageVersion: {version}
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
- RelativeFilePath: PhoneBoxAgent.exe
  PortableCommandAlias: phonebox
Dependencies:
  PackageDependencies:
  - PackageIdentifier: Google.PlatformTools
Installers:
- Architecture: x64
  InstallerUrl: {repo}/releases/download/v{version}/PhoneBoxAgent-{version}-win-x64.zip
  InstallerSha256: {sha}
ManifestType: installer
ManifestVersion: {schema}
""".lstrip('\n'), encoding='utf-8')

(out / f'{pkg}.locale.en-US.yaml').write_text(header.format(kind='defaultLocale') + f"""
PackageIdentifier: {pkg}
PackageVersion: {version}
PackageLocale: en-US
Publisher: {owner}
PublisherUrl: https://github.com/{owner}
PackageName: PhoneBox
PackageUrl: {repo}
License: MIT
LicenseUrl: {repo}/blob/main/LICENSE
ReleaseNotesUrl: {repo}/releases/tag/v{version}
ShortDescription: Turn an old Android phone into a desk panel for your PC.
Description: |-
  PhoneBox has two parts: an Android app that runs full screen on an old phone, and this agent, which lives in the
  tray and feeds it over USB or Wi-Fi: widgets for the PC, AI coding sessions and quotas, music, calendars,
  Home Assistant, OBS, Docker, webhooks and more. After installing, start PhoneBox, plug the phone in and use
  Settings › General › Install the phone app.
Tags:
- android
- dashboard
- desk-panel
- stream-deck
- widgets
- claude-code
ManifestType: defaultLocale
ManifestVersion: {schema}
""".lstrip('\n'), encoding='utf-8')
print(f'winget manifests written to {out}')
