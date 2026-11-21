"""Exercise the Unix installer with isolated command boundaries; safe on Linux and macOS."""
import argparse
import collections
import json
import os
import pathlib
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest

parser = argparse.ArgumentParser()
parser.add_argument('--bash', default='/bin/bash')
args = parser.parse_args()
INSTALLER = pathlib.Path(__file__).resolve().parents[1] / 'scripts/install.sh'
SETUP = INSTALLER.parents[1] / 'setup.sh'
Run = collections.namedtuple('Run', 'result calls apk cargo expected_cargo installed')

# The agent is the Rust build: a stray dotnet call fails at once.
CARGO_ONLY = 'echo "dotnet is not used any more" >&2\nexit 3\n'
# A fake `cargo build`: records where it ran and its arguments, and writes a fake agent that prints the install folder.
# It only ever writes below the test's folder, never into a real agent-rs/target.
CARGO_STUB = '''
case "${CARGO_TARGET_DIR:-}" in
  "$PHONEBOX_TEST_ROOT"/*) ;;
  *) echo "cargo stub: CARGO_TARGET_DIR is '${CARGO_TARGET_DIR:-}', not a folder of the test" >&2; exit 4 ;;
esac
{ pwd; printf '%s\\n' "$CARGO_TARGET_DIR" "$@"; } > "$PHONEBOX_TEST_ROOT/cargo-args.txt"
test "$1" = build || exit 2
mkdir -p "$CARGO_TARGET_DIR/release"
printf '%s\\n' '#!/bin/sh' 'printf "%s\\\\n" "$PHONEBOX_TEST_INSTALL_DIR"' > "$CARGO_TARGET_DIR/release/PhoneBoxAgent"
chmod +x "$CARGO_TARGET_DIR/release/PhoneBoxAgent"
'''


class SetupOptionTests(unittest.TestCase):
    def test_an_option_cannot_be_used_as_a_missing_serial_or_version(self):
        for option in ('--serial', '--version'):
            with self.subTest(option=option):
                result = subprocess.run([args.bash, str(SETUP), option, '--dry-run', '--help'],
                                        capture_output=True, text=True, timeout=5)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f'{option} needs a value', result.stderr)

    def test_valid_serial_and_version_are_accepted(self):
        result = subprocess.run([args.bash, str(SETUP), '--serial', 'fixture-device', '--version', '0.9.10', '--help'],
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('Options:', result.stdout)


class InstallerTests(unittest.TestCase):
    def test_missing_source_installer_serial_preserves_following_options(self):
        for flags in (('--serial', '--apk', '--help'), ('--serial', '', '--help'), ('--serial',)):
            with self.subTest(flags=flags):
                result = subprocess.run([args.bash, str(INSTALLER), *flags],
                                        capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertEqual(result.stderr.strip(), '--serial needs a value')
                self.assertEqual(result.stdout, '')

    def run_installer(self, serial=None, fail=False, newer_sources=False, target='cargo target'):
        """Runs a copy of install.sh in a fixture repository; `target` is CARGO_TARGET_DIR below the fixture
        (None leaves it unset, so the build goes to the repository's agent-rs/target)."""
        with tempfile.TemporaryDirectory(prefix='phonebox installer ') as directory:
            root = pathlib.Path(directory)
            repo = root / 'repo'
            (repo / 'scripts').mkdir(parents=True)
            (repo / 'dist').mkdir()
            (repo / 'agent-rs').mkdir()
            (repo / 'agent-rs/THIRD-PARTY-NOTICES.txt').write_text('fixture notices', encoding='utf-8')
            shutil.copyfile(INSTALLER, repo / 'scripts/install.sh')
            apk = repo / 'dist/PhoneBox-fixture.apk'
            apk.write_bytes(b'installer test fixture')
            if newer_sources:
                android = repo / 'android'
                source = android / 'app/src/main/AndroidManifest.xml'
                source.parent.mkdir(parents=True)
                source.write_text('<manifest/>', encoding='utf-8')
                os.utime(apk, (1, 1))
                (android / 'gradlew').write_text(
                    '#!/bin/sh\n'
                    'mkdir -p app/build/outputs/apk/release\n'
                    'printf %s built > app/build/outputs/apk/release/app-release.apk\n',
                    encoding='utf-8',
                )
                apk = android / 'app/build/outputs/apk/release/app-release.apk'

            commands = root / 'bin'
            commands.mkdir()

            def executable(name, body):
                path = commands / name
                path.write_text('#!/bin/sh\n' + body, encoding='utf-8')
                path.chmod(0o755)

            executable('uname', 'case "$1" in -s) echo Darwin;; -m) echo arm64;; *) exit 1;; esac\n')
            # These boundaries must never reach the real machine's agents or services.
            for name in ('pkill', 'nohup', 'codesign', 'systemctl', 'launchctl'):
                executable(name, 'exit 0\n')
            executable('dotnet', CARGO_ONLY)
            executable('cargo', CARGO_STUB)
            recorder = root / 'record-adb.py'
            recorder.write_text('''import json, os, pathlib, sys
with pathlib.Path(os.environ['PHONEBOX_TEST_CALLS']).open('a') as stream:
    stream.write(json.dumps(sys.argv[1:]) + '\\n')
if 'install' in sys.argv[1:] and os.environ.get('PHONEBOX_TEST_FAIL') == '1':
    print('Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE]', file=sys.stderr)
    sys.exit(1)
print('Success')
''', encoding='utf-8')
            executable('adb', f'exec {shlex.quote(sys.executable)} {shlex.quote(str(recorder))} "$@"\n')
            calls_path = root / 'adb.jsonl'
            installed = root / 'installed'
            env = dict(os.environ, PATH=str(commands) + os.pathsep + os.environ['PATH'],
                       TMPDIR=str(root), PHONEBOX_TEST_ROOT=str(root),
                       PHONEBOX_TEST_INSTALL_DIR=str(installed),
                       PHONEBOX_TEST_CALLS=str(calls_path), PHONEBOX_TEST_FAIL='1' if fail else '0')
            env.pop('CARGO_TARGET_DIR', None)
            if target is not None:
                env['CARGO_TARGET_DIR'] = str(root / target)
            command = [args.bash, str(repo / 'scripts/install.sh'), '--apk']
            if serial is not None:
                command += ['--serial', serial]
            result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=20)
            calls = [json.loads(line) for line in calls_path.read_text().splitlines()] if calls_path.exists() else []
            cargo_path = root / 'cargo-args.txt'
            cargo = cargo_path.read_text(encoding='utf-8').splitlines() if cargo_path.exists() else []
            if cargo:  # the folder cargo ran in, compared without symbolic links (macOS keeps temp folders in /private)
                cargo[0] = os.path.realpath(cargo[0])
            expected_cargo = [
                os.path.realpath(repo / 'agent-rs'),
                str(root / target) if target is not None else str(repo / 'agent-rs/target'),
                'build', '--release', '--locked', '--manifest-path', str(repo / 'agent-rs/Cargo.toml'),
                '-p', 'phonebox-agent',
            ]
            files = sorted(p.name for p in installed.iterdir()) if installed.is_dir() else []
            return Run(result, calls, str(apk), cargo, expected_cargo, files)

    def test_the_rust_agent_is_built_with_cargo_and_installed_with_its_notices(self):
        for target in ('cargo target', None):
            with self.subTest(CARGO_TARGET_DIR=target):
                run = self.run_installer(target=target)
                output = run.result.stdout + run.result.stderr
                self.assertEqual(run.result.returncode, 0, output)
                self.assertEqual(run.cargo, run.expected_cargo, output)
                self.assertEqual(run.installed, ['PhoneBoxAgent', 'THIRD-PARTY-NOTICES.txt'], output)

    def test_explicit_serial_targets_only_that_phone(self):
        result, calls, apk = self.run_installer(serial='fixture-device')[:3]
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, [
            ['-s', 'fixture-device', 'install', '-r', apk],
            ['-s', 'fixture-device', 'shell', 'am', 'start', '-n', 'net.oxog.phonebox.deskpanel/.MainActivity'],
        ], result.stdout + result.stderr)

    def test_no_serial_installs_and_opens_the_phone_app(self):
        result, calls, apk = self.run_installer()[:3]
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, [
            ['install', '-r', apk],
            ['shell', 'am', 'start', '-n', 'net.oxog.phonebox.deskpanel/.MainActivity'],
        ], result.stdout + result.stderr)

    def test_newer_android_sources_are_built_before_install(self):
        result, calls, apk = self.run_installer(newer_sources=True)[:3]
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('Building the APK', result.stdout)
        self.assertEqual(calls[0], ['install', '-r', apk])

    def test_failed_update_preserves_app_without_uninstall_or_launch(self):
        result, calls, apk = self.run_installer(serial='fixture-device', fail=True)[:3]
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('INSTALL_FAILED_UPDATE_INCOMPATIBLE', result.stderr)
        self.assertEqual(calls, [['-s', 'fixture-device', 'install', '-r', apk]])


if __name__ == '__main__':
    unittest.main(argv=[sys.argv[0]], verbosity=2)
