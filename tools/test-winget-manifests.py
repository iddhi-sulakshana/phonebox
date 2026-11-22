"""Exercise release manifest generation without writing a real version."""
import pathlib
import shutil
import subprocess
import sys
import tempfile

GENERATOR = pathlib.Path(__file__).resolve().parents[1] / 'packaging/winget/make-manifests.py'
PKG = 'iddhi-sulakshana.PhoneBox'  # this fork's identifier since 1.0.0 (upstream's is ersinkoc.PhoneBox)
REPO = 'https://github.com/iddhi-sulakshana/phonebox'

with tempfile.TemporaryDirectory() as tmp:
    generator = pathlib.Path(shutil.copyfile(GENERATOR, pathlib.Path(tmp, GENERATOR.name)))
    version = '0.0.1'
    subprocess.run([sys.executable, str(generator), version, 'a' * 64], check=True)
    directory = pathlib.Path(tmp, 'manifests/i/iddhi-sulakshana/PhoneBox', version)
    assert sorted(p.name for p in directory.iterdir()) == sorted(
        [f'{PKG}.yaml', f'{PKG}.installer.yaml', f'{PKG}.locale.en-US.yaml']), sorted(directory.iterdir())
    locale = (directory / f'{PKG}.locale.en-US.yaml').read_text(encoding='utf-8')
    installer = (directory / f'{PKG}.installer.yaml').read_text(encoding='utf-8')
    notes = f'ReleaseNotesUrl: {REPO}/releases/tag/v{version}'
    assert notes in locale.splitlines(), 'WinGet default locale must carry the release notes URL'
    assert notes not in installer.splitlines(), 'WinGet installer has no release notes field'
    for text in (locale, installer, (directory / f'{PKG}.yaml').read_text(encoding='utf-8')):
        assert f'PackageIdentifier: {PKG}' in text.splitlines(), 'every manifest names the fork identifier'
        assert 'ersinkoc' not in text, 'no manifest points at the original project'
    assert (f'  InstallerUrl: {REPO}/releases/download/v{version}/PhoneBoxAgent-{version}-win-x64.zip'
            in installer.splitlines()), 'the installer downloads the fork release'
    print('PASS: WinGet manifests use the fork identifier; release notes live in the default locale')
