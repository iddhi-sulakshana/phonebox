"""Tests for the release version checks: tools/check-release.py, tools/check-notices.py and tools/pages/build.py.

The release version is agent-rs/Cargo.toml's; Android's versionName must equal it; a final version (no -alpha.N) is
refused unless the release asks for it (`--final`). Run: python3 -I tools/test-release-metadata.py
"""
import importlib.util
import pathlib
import shutil
import sys
import tempfile

sys.dont_write_bytecode = True
TOOLS = pathlib.Path(__file__).resolve().parent


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


release = load('check_release', TOOLS / 'check-release.py')
zipper = load('zip_agent', TOOLS / 'zip-agent.py')
notices = load('check_notices', TOOLS / 'check-notices.py')
pages = load('pages_build', TOOLS / 'pages' / 'build.py')


def fixture(root, cargo='1.0.0-alpha.1', android='1.0.0-alpha.1', code=100, heading=None):
    root = pathlib.Path(root)
    (root / 'agent-rs').mkdir()
    (root / 'agent-rs' / 'Cargo.toml').write_text(
        '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "%s"\nedition = "2024"\n' % cargo, encoding='utf-8')
    (root / 'android' / 'app').mkdir(parents=True)
    (root / 'android' / 'app' / 'build.gradle.kts').write_text(
        'android {\n    defaultConfig {\n        versionCode = %d\n        versionName = "%s"\n    }\n}\n' % (code, android),
        encoding='utf-8')
    heading = heading if heading is not None else f'## {cargo} — 2026-11-22'
    (root / 'CHANGELOG.md').write_text(f'# Changelog\n\n{heading}\n\n- Notes.\n', encoding='utf-8')
    return root


def refused(check, *args, **kwargs):
    try:
        check(*args, **kwargs)
    except AssertionError as error:
        return str(error)
    raise AssertionError('accepted: %r %r' % (args, kwargs))


def test_an_alpha_from_cargo_with_the_same_android_version_passes():
    with tempfile.TemporaryDirectory() as tmp:
        assert release.check_version(fixture(tmp)) == '1.0.0-alpha.1'


def test_a_different_android_version_is_refused():
    with tempfile.TemporaryDirectory() as tmp:
        assert 'Android' in refused(release.check_version, fixture(tmp, android='0.9.23'))


def test_a_final_version_needs_the_final_flag():
    with tempfile.TemporaryDirectory() as tmp:
        root = fixture(tmp, cargo='1.0.0', android='1.0.0')
        assert 'final' in refused(release.check_version, root)
        assert release.check_version(root, final=True) == '1.0.0'


def test_other_pre_releases_and_a_missing_dated_heading_are_refused():
    for cargo in ('1.0.0-beta.1', '1.0.0-alpha.0', '1.0.0-alpha', '1.0'):
        with tempfile.TemporaryDirectory() as tmp:
            refused(release.check_version, fixture(tmp, cargo=cargo, android=cargo), final=True)
    with tempfile.TemporaryDirectory() as tmp:
        assert 'release notes' in refused(release.check_version, fixture(tmp, heading='## Unreleased'))
    with tempfile.TemporaryDirectory() as tmp:
        refused(release.check_version, fixture(tmp, code=0))


# `cargo tree --prefix none --format {p}`: the workspace's crates carry their folder, repeats end in (*)
TREE = """phonebox-agent v1.0.0-alpha.1 (/work/agent-rs/crates/phonebox-agent)
phonebox v1.0.0-alpha.1 (/work/agent-rs/crates/phonebox)
serde v1.0.228
ring v0.17.14
serde v1.0.228 (*)
"""


def test_the_notices_must_name_every_crate_of_the_build():
    complete = 'MIT\n\nUsed by:\n- serde 1.0.228\n- ring 0.17.14 (https://github.com/briansmith/ring)\n'
    assert notices.missing(TREE, complete) == []
    # the workspace's own crates need no notice
    assert notices.crates(TREE) == ['serde 1.0.228', 'ring 0.17.14']
    assert notices.missing(TREE, complete.replace('- ring 0.17.14', '- ring 0.17.13')) == ['ring 0.17.14']
    assert notices.missing(TREE, 'serde 1.0.228 ring 0.17.14') == ['serde 1.0.228', 'ring 0.17.14']


def test_the_committed_notices_name_every_crate_of_the_build():
    if shutil.which('cargo') is None:
        print('     (no cargo here: tools/check-notices.py runs in the rust workflow)')
        return
    text = (TOOLS.parent / 'agent-rs' / 'THIRD-PARTY-NOTICES.txt').read_text(encoding='utf-8')
    assert notices.missing(notices.tree(), text) == []


def test_the_pages_show_the_cargo_version():
    with tempfile.TemporaryDirectory() as tmp:
        root = fixture(tmp, cargo='1.0.0-alpha.7', android='1.0.0-alpha.7')
        saved = pages.ROOT
        pages.ROOT = root
        try:
            assert pages.version() == '1.0.0-alpha.7'
        finally:
            pages.ROOT = saved


def test_the_agent_zip_holds_the_two_files_at_its_root():
    import os
    import zipfile
    with tempfile.TemporaryDirectory() as tmp:
        out = pathlib.Path(tmp, 'out')
        out.mkdir()
        (out / 'PhoneBoxAgent').write_bytes(b'\x7fELF')
        (out / 'PhoneBoxAgent').chmod(0o755)
        (out / 'THIRD-PARTY-NOTICES.txt').write_text('notices', encoding='utf-8')
        target = pathlib.Path(tmp, 'PhoneBoxAgent-1.0.0-alpha.1-linux-x64.zip')
        zipper.zip_agent(out, target)
        with zipfile.ZipFile(target) as z:
            assert sorted(z.namelist()) == ['PhoneBoxAgent', 'THIRD-PARTY-NOTICES.txt']
            if os.name != 'nt':
                # unzipped by setup.sh or the updater, the agent can run
                assert (z.getinfo('PhoneBoxAgent').external_attr >> 16) & 0o111
        (out / 'extra.dll').write_bytes(b'')
        refused(zipper.zip_agent, out, pathlib.Path(tmp, 'other.zip'))


if __name__ == '__main__':
    tests = [(n, f) for n, f in sorted(globals().items()) if n.startswith('test_') and callable(f)]
    failed = 0
    for name, test in tests:
        try:
            test()
            print(f'ok   {name}')
        except Exception as error:  # noqa: BLE001 - report every failure, then exit non-zero
            failed += 1
            print(f'FAIL {name}: {type(error).__name__}: {error}')
    print(f'{len(tests) - failed} passed, {failed} failed')
    sys.exit(1 if failed else 0)
