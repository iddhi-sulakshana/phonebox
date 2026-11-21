"""Check that agent-rs/THIRD-PARTY-NOTICES.txt names every crate the agent is built from:
python3 tools/check-notices.py   (needs cargo)

The notices ship in every agent zip. After a dependency change, regenerate them with agent-rs/scripts/notices.sh.
The crates are those `cargo tree` lists for the release targets of agent-rs/about.toml (normal, build and dev
dependencies), which applies Cargo's feature resolver as a build does. Cargo.lock and `cargo metadata` also hold
optional and weak dependencies that no feature turns on; they never ship and need no notice.
"""
import pathlib, subprocess, sys, tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]


def crates(tree_text):
    """"name version" for each `cargo tree --prefix none --format {p}` line, but the workspace's own crates (shown with
    their folder in parentheses)."""
    out = []
    for line in tree_text.splitlines():
        line = line.strip().removesuffix(' (*)').removesuffix(' (proc-macro)')
        if not line or '(' in line:
            continue
        name, _, version = line.partition(' v')
        entry = f'{name} {version}'
        if entry not in out:
            out.append(entry)
    return out


def missing(tree_text, notices_text):
    """The crates without a "- name version" line in the notices."""
    named = {line.strip().split(' (', 1)[0] for line in notices_text.splitlines() if line.startswith('- ')}
    return [c for c in crates(tree_text) if f'- {c}' not in named]


def tree(root=ROOT):
    agent = root / 'agent-rs'
    targets = tomllib.loads((agent / 'about.toml').read_text(encoding='utf-8'))['targets']
    command = ['cargo', 'tree', '--locked', '--workspace', '-e', 'normal,build,dev', '--prefix', 'none', '--format', '{p}']
    for target in targets:
        command += ['--target', target]
    return subprocess.run(command, check=True, capture_output=True, text=True, cwd=agent).stdout


def main():
    listed = tree()
    notices = (ROOT / 'agent-rs/THIRD-PARTY-NOTICES.txt').read_text(encoding='utf-8')
    absent = missing(listed, notices)
    if absent:
        print('THIRD-PARTY-NOTICES.txt lacks ' + ', '.join(absent) + ': run agent-rs/scripts/notices.sh')
        sys.exit(1)
    print(f'PASS: the notices name all {len(crates(listed))} crates the agent is built from')


if __name__ == '__main__':
    main()
