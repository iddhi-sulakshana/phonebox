"""Exercise generated Unix login configuration without touching real services."""
import os
import pathlib
import plistlib
import subprocess
import sys
import tempfile
import unittest

INSTALL = pathlib.Path(__file__).resolve().parents[1] / 'scripts/install.sh'


class AutostartTests(unittest.TestCase):
    def install(self, os_name, root):
        commands = root / 'bin'
        commands.mkdir()

        def executable(name, program):
            path = commands / name
            path.write_text('#!/bin/sh\n' + program, encoding='utf-8')
            path.chmod(0o755)

        executable('uname', f'case "$1" in -s) echo {os_name};; -m) echo arm64;; esac\n')
        for name in ('pkill', 'codesign', 'systemctl', 'launchctl'):
            executable(name, 'exit 0\n')
        # the agent is the Rust build: a stray dotnet call fails at once
        executable('dotnet', 'echo "dotnet is not used any more" >&2\nexit 3\n')
        # This runs the repository's own install.sh: the fake `cargo build` writes only below the test's folder
        # (CARGO_TARGET_DIR below), never into the real agent-rs/target.
        executable('cargo', """
case "${CARGO_TARGET_DIR:-}" in
  "$PHONEBOX_TEST_ROOT"/*) ;;
  *) echo "cargo stub: CARGO_TARGET_DIR is '${CARGO_TARGET_DIR:-}', not a folder of the test" >&2; exit 4 ;;
esac
test "$1" = build || exit 2
mkdir -p "$CARGO_TARGET_DIR/release"
printf '%s\\n' '#!/bin/sh' 'printf "%s\\\\n" "$PHONEBOX_TEST_INSTALL_DIR"' > "$CARGO_TARGET_DIR/release/PhoneBoxAgent"
chmod +x "$CARGO_TARGET_DIR/release/PhoneBoxAgent"
""")
        app = root / ('PhoneBox & extras' if os_name == 'Darwin' else 'PhoneBox 100%')
        env = dict(
            os.environ,
            PATH=str(commands) + os.pathsep + os.environ['PATH'],
            HOME=str(root),
            TMPDIR=str(root),
            PHONEBOX_TEST_ROOT=str(root),
            CARGO_TARGET_DIR=str(root / 'cargo target'),
            # left over from a C# install: it must not reach the service files any more
            DOTNET_ROOT=str(root / 'dotnet & runtime'),
            XDG_DATA_HOME=str(root / 'data directory'),
            XDG_CONFIG_HOME=str(root / 'config directory'),
            PHONEBOX_TEST_INSTALL_DIR=str(app),
        )
        bash = sys.argv[1] if len(sys.argv) > 1 else '/bin/bash'
        result = subprocess.run(
            [bash, str(INSTALL), '--autostart'],
            env=env, capture_output=True, text=True, timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return app, env

    def test_linux_service_quotes_paths_and_preserves_xdg(self):
        with tempfile.TemporaryDirectory(prefix='phonebox ') as tmp:
            root = pathlib.Path(tmp)
            app, env = self.install('Linux', root)
            unit = (root / '.config/systemd/user/phonebox.service').read_text(encoding='utf-8')
            self.assertIn(f'ExecStart="{app / "PhoneBoxAgent"}"'.replace('%', '%%'), unit)
            for name in ('XDG_DATA_HOME', 'XDG_CONFIG_HOME'):
                self.assertIn(f'Environment="{name}={env[name]}"', unit)
            self.assertNotIn('DOTNET_ROOT', unit)

    def test_macos_launch_agent_keeps_xml_metacharacters_in_paths(self):
        with tempfile.TemporaryDirectory(prefix='phonebox ') as tmp:
            root = pathlib.Path(tmp)
            app, env = self.install('Darwin', root)
            plist = root / 'Library/LaunchAgents/net.oxog.phonebox.agent.plist'
            with plist.open('rb') as stream:
                config = plistlib.load(stream)
            self.assertEqual(config['ProgramArguments'], [str(app / 'PhoneBoxAgent')])
            self.assertNotIn('DOTNET_ROOT', plist.read_text(encoding='utf-8'))


if __name__ == '__main__':
    unittest.main(argv=[sys.argv[0]])
