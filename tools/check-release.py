"""Validate the source metadata and catalog shipped in every release:  python3 tools/check-release.py [--final]

The release version is agent-rs/Cargo.toml's. Until the switch it is 1.0.0-alpha.N; a final version (no "-") is refused
unless the release asks for it with --final (the release workflow's `final` input: ask the owner before 1.0.0).
"""
import json, pathlib, re, sys, tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
VERSION = re.compile(r'\d+\.\d+\.\d+(-alpha\.[1-9]\d*)?')


def cargo_version(root):
    return tomllib.loads((root / 'agent-rs/Cargo.toml').read_text(encoding='utf-8'))['workspace']['package']['version']


def check_version(root, final=False):
    """The Cargo version, after checking Android's versionName and versionCode and the dated CHANGELOG heading."""
    version = cargo_version(root)
    assert VERSION.fullmatch(version), f'{version} is neither x.y.z nor x.y.z-alpha.N'
    assert final or '-' in version, f'{version} is a final version: release it with the final input (ask the owner first)'
    gradle = (root / 'android/app/build.gradle.kts').read_text(encoding='utf-8')
    assert re.search(r'versionName = "' + re.escape(version) + '"', gradle), f'Android versionName differs from {version}'
    assert int(re.search(r'versionCode = (\d+)', gradle)[1]) > 0, 'Android versionCode must be positive'
    changelog = (root / 'CHANGELOG.md').read_text(encoding='utf-8')
    assert re.search(r'^## ' + re.escape(version) + r' — \d{4}-\d{2}-\d{2}$', changelog, re.M), \
        f'Missing dated release notes: ## {version} — YYYY-MM-DD in CHANGELOG.md'
    return version


def check_catalog(root, version):
    widgets = json.loads((root / 'agent-rs/assets/widgets.json').read_text(encoding='utf-8'))
    templates = json.loads((root / 'agent-rs/assets/templates.json').read_text(encoding='utf-8'))
    types = {w['type'] for w in widgets}
    assert len(types) == len(widgets), 'Duplicate widget types'
    assert len({t['id'] for t in templates}) == len(templates), 'Duplicate templates'
    for t in templates:
        for w in t['widgets']:
            assert w['type'] in types, (t['id'], w['type'])
            assert 0 <= w.get('x', 0) and 0 <= w.get('y', 0) and w['x'] + w['w'] <= 12 and w['y'] + w['h'] <= 6, (t['id'], w)
    readme = (root / 'README.md').read_text(encoding='utf-8')
    import importlib.util
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location('pages_build', root / 'tools/pages/build.py')
    build = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(build)
    site = build.landing_html()
    assert f'**{len(widgets)} widgets**' in readme and f'**{len(templates)} templates**' in readme, 'README catalog counts differ'
    assert f'{len(widgets)} widgets, {len(templates)} templates' in site and f'PhoneBox {version}' in site, \
        'Demo catalog counts or version differ'
    # the landing page takes its numbers from the sources; a number typed by hand would go stale
    assert not re.search(r'\d+ (widgets|templates)', (root / 'tools/pages/index.html').read_text(encoding='utf-8')), \
        'Hard-coded catalog count in the landing page'
    demo = (root / 'tools/pages/demo.js').read_text(encoding='utf-8')
    pages = json.loads(re.search(r'const PAGES = (\[[^\]]*\])', demo)[1].replace("'", '"'))
    assert sorted(pages) == sorted(t['id'] for t in templates), 'The demo does not show every template exactly once'
    return len(widgets), len(templates)


def main(argv):
    version = check_version(ROOT, final='--final' in argv)
    widgets, templates = check_catalog(ROOT, version)
    print(f'PASS: {version}; Android metadata; {widgets} widgets; {templates} templates; README/demo counts')


if __name__ == '__main__':
    main(sys.argv[1:])
