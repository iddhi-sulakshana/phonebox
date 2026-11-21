"""The config a resource run gives each agent: the default config for Linux with no phone, no automatic updates and no
charge guard, and Wi-Fi on with the fixture's port and certificate but keys of its own (the fixture's keys are in the
repository, and the agents listen on every interface of the test server).

Usage: python3 -I tools/resources/make_config.py <config.default.linux.json> <lan fixture.json> <wifi port> <out.json>
"""
import json
import re
import secrets
import string
import sys


def strip_jsonc(text):
    out, i, n, in_str = [], 0, len(text), False
    while i < n:
        c = text[i]
        if in_str:
            out.append(text[i:i + 2] if c == "\\" else c)
            i += 2 if c == "\\" else 1
            in_str = c != '"'
            continue
        if c == '"':
            in_str = True
        elif text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
            continue
        elif text.startswith("/*", i):
            j = text.find("*/", i + 2)
            i = n if j < 0 else j + 2
            continue
        out.append(c)
        i += 1
    return re.sub(r",(\s*[}\]])", r"\1", "".join(out))


def resource_config(default_text: str, lan: dict, port: int) -> dict:
    cfg = json.loads(strip_jsonc(default_text))
    cfg["deviceSerial"] = "phonebox-resource-no-phone"
    cfg.setdefault("updates", {})["auto"] = False
    cfg.setdefault("charge", {})["enabled"] = False
    cfg["lan"] = dict(lan, port=port, token=fresh_key(), webhookToken=fresh_key())
    return cfg


def fresh_key() -> str:
    alphabet = string.ascii_letters + string.digits
    return "".join(secrets.choice(alphabet) for _ in range(43))


def main(argv):
    default_text = open(argv[1], encoding="utf-8").read()
    lan = json.load(open(argv[2], encoding="utf-8"))["lan"]
    cfg = resource_config(default_text, lan, int(argv[3]))
    open(argv[4], "w", encoding="utf-8").write(json.dumps(cfg, indent=2) + "\n")


if __name__ == "__main__":
    main(sys.argv)
