"""Zips a release agent:  python3 tools/zip-agent.py <folder> <zip>   (Python 3.8 or later, standard library only)

The folder must hold exactly PhoneBoxAgent (PhoneBoxAgent.exe on Windows) and THIRD-PARTY-NOTICES.txt. Both go to the
zip's root, where setup and the updater look for them, and the executable keeps its Unix mode, so an unzipped agent
can run.
"""
import pathlib, sys, zipfile


def zip_agent(folder, target):
    folder = pathlib.Path(folder)
    names = sorted(p.name for p in folder.iterdir())
    exe = 'PhoneBoxAgent.exe' if 'PhoneBoxAgent.exe' in names else 'PhoneBoxAgent'
    assert names == sorted([exe, 'THIRD-PARTY-NOTICES.txt']), f'{folder} holds {names}'
    with zipfile.ZipFile(target, 'w', zipfile.ZIP_DEFLATED) as z:
        for name in names:
            z.write(folder / name, name)  # with the file's mode
    with zipfile.ZipFile(target) as z:
        assert sorted(z.namelist()) == names, z.namelist()
    return names


if __name__ == '__main__':
    print(zip_agent(sys.argv[1], sys.argv[2]))
