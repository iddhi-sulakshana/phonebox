"""Exercise the one-step Unix setup with synthetic release, phone, and agent."""
import hashlib
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest
from zipfile import ZipFile

SETUP = Path(__file__).resolve().parents[1] / 'setup.sh'


class SetupPhoneTests(unittest.TestCase):
    def install(self, mode, root, expected_code=0, version='0.0.1', args=None, releases=None, feed=None):
        bin_dir = root / 'bin'
        bin_dir.mkdir()
        agent = root / 'PhoneBox/app'
        archive = root / f'PhoneBoxAgent-{version}-linux-x64.zip'
        apk = root / f'PhoneBox-{version}.apk'
        apk.write_bytes(b'fixture APK')
        with ZipFile(archive, 'w') as bundle:
            bundle.writestr('PhoneBoxAgent', '#!/bin/sh\nprintf "%s\\n" "$PHONEBOX_TEST_AGENT"\n')
            bundle.writestr('THIRD-PARTY-NOTICES.txt', 'notices')
        # the GitHub API's release list (newest first), for --pre
        if releases is not None:
            (root / 'releases.json').write_text(releases, encoding='utf-8')
        # the release page's Atom feed (newest first, pre-releases included; no API limit)
        if feed is not None:
            (root / 'releases.atom').write_text(feed, encoding='utf-8')
        (root / f'SHA256SUMS-{version}.txt').write_text(
            ''.join(f'{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n'
                    for p in (archive, apk)),
            encoding='utf-8',
        )

        def stub(name, command):
            executable = bin_dir / name
            executable.write_text('#!/bin/sh\n' + command + '\n', encoding='utf-8')
            executable.chmod(0o755)

        downloader = root / 'curl.py'
        downloader.write_text("""
import os, pathlib, shutil, sys
args = sys.argv[1:]
url = args[-1]
if url == 'http://127.0.0.1:8765/api/info':
    sys.exit(0)
root = pathlib.Path(os.environ['PHONEBOX_TEST_ROOT'])
if url.startswith('https://api.github.com/'):
    # only the release list exists here (no full release yet, so /releases/latest is a 404)
    listed = root / 'releases.json'
    if '/releases?' not in url or not listed.exists():
        sys.exit(22)
    sys.stdout.write(listed.read_text())
    sys.exit(0)
if url.endswith('/releases.atom'):
    feed = root / 'releases.atom'
    if not feed.exists():
        sys.exit(22)
    sys.stdout.write(feed.read_text())
    sys.exit(0)
if '-o' not in args:
    sys.exit(22)
source = root / url.rsplit('/', 1)[-1]
dest = pathlib.Path(args[args.index('-o') + 1])
if source.suffix == '.apk' and os.environ['PHONEBOX_TEST_MODE'] == 'missing':
    sys.exit(22)
mode = os.environ['PHONEBOX_TEST_MODE']
if (source.suffix, mode) in (('.apk', 'corrupt'), ('.zip', 'corrupt-agent')):
    dest.write_bytes(b'damaged download')
else:
    shutil.copyfile(source, dest)
""", encoding='utf-8')
        stub('curl', f'exec {shlex.quote(sys.executable)} {shlex.quote(str(downloader))} "$@"')
        stub('uname', 'case "$1" in -s) echo Linux;; -m) echo x86_64;; -r) echo fixture;; esac')
        stub('id', 'echo 1000')
        stub('adb', '''case "$1" in
  devices) printf 'List of devices attached\nfixture-phone\tdevice\n' ;;
  -s) case "$3:$4" in
        shell:getprop) echo 34 ;;
        install:-r) touch "$PHONEBOX_TEST_ROOT/phone-installed"; echo Success ;;
      esac ;;
esac''')
        stub('nohup', 'touch "$PHONEBOX_TEST_ROOT/agent-started"')
        for name in ('gh', 'systemctl', 'pgrep', 'ldd'):
            stub(name, 'exit 1')
        for name in ('pkill', 'sleep'):
            stub(name, 'exit 0')
        env = dict(os.environ, PATH=str(bin_dir) + os.pathsep + os.environ['PATH'],
                   HOME=str(root), TMPDIR=str(root), PHONEBOX_TEST_ROOT=str(root),
                   PHONEBOX_TEST_AGENT=str(agent), PHONEBOX_TEST_MODE=mode,
                   DISPLAY='', WAYLAND_DISPLAY='')
        if args is None:
            args = ['--version', version]
        result = subprocess.run(
            ['/bin/bash', str(SETUP), '--yes', '--no-autostart', '--no-extras', *args],
            env=env, capture_output=True, text=True, timeout=20,
        )
        self.assertEqual(result.returncode, expected_code, result.stdout + result.stderr)
        if expected_code == 0:
            # the release assets came from this repository's downloads
            self.assertIn(f'PhoneBox {version} (linux-x64)', result.stdout)
        self.assertEqual((agent / 'PhoneBoxAgent').exists(), expected_code == 0)
        self.assertEqual((root / 'agent-started').exists(), expected_code == 0,
                         result.stdout + result.stderr)
        return result

    def test_download_and_verification_failures_keep_desktop_running(self):
        for mode in ('missing', 'corrupt'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                result = self.install(mode, root)
                self.assertIn('Phone app skipped', result.stdout)
                self.assertIn('PhoneBox is running', result.stdout)
                self.assertFalse((root / 'phone-installed').exists())

    def test_unverified_agent_still_blocks_installation(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.install('corrupt-agent', Path(directory), expected_code=1)
            self.assertIn('the agent download could not be verified', result.stderr)

    def test_verified_apk_still_reaches_the_phone(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.install('valid', root)
            self.assertTrue((root / 'phone-installed').exists())


    def test_a_pre_release_installs_by_its_version(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.install('valid', root, version='1.0.0-alpha.2')
            self.assertTrue((root / 'phone-installed').exists())

    def test_pre_installs_the_newest_release_including_pre_releases(self):
        listed = ('[{"tag_name": "v1.0.0-alpha.2", "draft": false, "prerelease": true},'
                  ' {"tag_name": "v1.0.0-alpha.1", "draft": false, "prerelease": true}]')
        with tempfile.TemporaryDirectory() as directory:
            self.install('valid', Path(directory), version='1.0.0-alpha.2', args=['--pre'], releases=listed)

    def test_pre_falls_back_to_the_release_feed_when_the_api_is_rate_limited(self):
        feed = ('<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom">'
                '<entry><link rel="alternate" type="text/html" '
                'href="https://github.com/iddhi-sulakshana/phonebox/releases/tag/v1.0.0-alpha.2"/></entry>'
                '<entry><link rel="alternate" type="text/html" '
                'href="https://github.com/iddhi-sulakshana/phonebox/releases/tag/v1.0.0-alpha.1"/></entry></feed>')
        with tempfile.TemporaryDirectory() as directory:
            # no releases.json: every API request fails, as when the hourly limit is used up
            self.install('valid', Path(directory), version='1.0.0-alpha.2', args=['--pre'], feed=feed)

    def test_without_a_full_release_setup_names_the_pre_option(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.install('valid', Path(directory), expected_code=1, args=[])
            self.assertIn('--pre', result.stderr)

    def test_a_malformed_version_is_refused(self):
        for version in ('1.0.0-', '1.0', '1.0.0-alpha_1'):
            with self.subTest(version=version), tempfile.TemporaryDirectory() as directory:
                result = self.install('valid', Path(directory), expected_code=1, args=['--version', version])
                self.assertIn('unexpected version', result.stderr)


if __name__ == '__main__':
    unittest.main()
