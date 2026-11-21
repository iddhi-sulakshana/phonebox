#!/usr/bin/env python3
"""Parity harness: runs the C# and the Rust PhoneBox agents side by side and diffs their HTTP answers.

    python3 tools/parity/parity.py --csharp "agent/PhoneBox.Agent/bin/Release/net10.0/PhoneBoxAgent" \
        --rust agent-rs/target/release/PhoneBoxAgent --corpus tools/parity/corpus/phase1.json

Each agent gets its own copy of the fixture config in a temporary folder and a free port, so neither touches an
installed agent or a phone (the fixture sets deviceSerial to an unmatched value). Standard library only.

Corpus: a JSON list of requests. Fields:
  name      label in reports
  method    GET, POST, PUT, DELETE (default GET)
  path      request path and query; "{port}" becomes each agent's port
  headers   extra headers ("{port}" filled in); a "Host" here replaces the default 127.0.0.1:<port>
  body      request body text or null
  ignore    JSON pointers removed from both bodies before comparing ("*" matches every array item / object key)
  shape     JSON pointers compared by shape only: object keys (in order), array lengths and which values are objects
            or arrays; numbers, strings, booleans and null all count as the same (live readings such as CPU or ping)
  wait      seconds to wait before sending this request (lets the agents' monitors take their first samples)
  compare   "json" (default), "bytes" or "status"
  config_version  true: send X-Config-Version with the value GET /api/settings just returned on that agent
  config_file     true: also compare the agents' config.json files byte for byte after the request
  wifi      true: send over HTTPS to the agent's Wi-Fi port (without verifying the certificate; its SHA-256 is compared
            as the pseudo-header x-peer-fingerprint); "{lanport}" becomes each agent's Wi-Fi port

A fixture with Wi-Fi on ("lan": {"enabled": true}) gives each agent its own Wi-Fi port in its copy of the config and
the fixture certificate (fixtures/lan-cert.pfx, or --fixture-cert) next to it, so both serve the same certificate.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import http.client
import json
import os
import shlex
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

# Response headers that are part of the contract (security and caching); compared whenever either side sends one.
CONTRACT_HEADERS = (
    "content-type", "cache-control", "x-frame-options", "x-content-type-options", "content-security-policy",
    "x-config-version-present", "x-peer-fingerprint",
)


@dataclass
class Response:
    status: int
    headers: dict[str, str]
    body: bytes


# ---------------------------------------------------------------- pure comparison logic (unit-tested)
def fill(text: str, values: dict) -> str:
    for key, value in values.items():
        text = text.replace("{" + key + "}", str(value))
    return text


def wifi_enabled(config_text: str) -> bool:
    lan = json.loads(config_text).get("lan")
    return isinstance(lan, dict) and lan.get("enabled") is True


def prepare_config(config_text: str, lan_port: int) -> str:
    """The agent's copy of the fixture: with Wi-Fi on it gets the agent's own Wi-Fi port; otherwise it is unchanged."""
    if not wifi_enabled(config_text):
        return config_text
    cfg = json.loads(config_text)
    cfg["lan"]["port"] = lan_port
    return json.dumps(cfg, indent=2) + "\n"


def _remove(doc, parts: list[str]):
    if not parts:
        return
    head, rest = parts[0], parts[1:]
    if isinstance(doc, dict):
        keys = list(doc.keys()) if head == "*" else ([head] if head in doc else [])
        for k in keys:
            if rest:
                _remove(doc[k], rest)
            else:
                del doc[k]
    elif isinstance(doc, list):
        if head == "*":
            idx = list(range(len(doc)))
        elif head.isdigit() and int(head) < len(doc):
            idx = [int(head)]
        else:
            idx = []
        for i in reversed(idx):
            if rest:
                _remove(doc[i], rest)
            else:
                del doc[i]


def strip_pointers(doc, pointers: list[str]):
    doc = copy.deepcopy(doc)
    for p in pointers:
        parts = [x.replace("~1", "/").replace("~0", "~") for x in p.split("/")[1:]]
        _remove(doc, parts)
    return doc


def shape_of(doc):
    """Objects keep their keys in order, arrays their length; every other value becomes None."""
    if isinstance(doc, dict):
        return {k: shape_of(v) for k, v in doc.items()}
    if isinstance(doc, list):
        return [shape_of(v) for v in doc]
    return None


def _shape(doc, parts: list[str]):
    head, rest = parts[0], parts[1:]
    if isinstance(doc, dict):
        keys = list(doc.keys()) if head == "*" else ([head] if head in doc else [])
    elif isinstance(doc, list):
        keys = list(range(len(doc))) if head == "*" else ([int(head)] if head.isdigit() and int(head) < len(doc) else [])
    else:
        keys = []
    for k in keys:
        if rest:
            _shape(doc[k], rest)
        else:
            doc[k] = shape_of(doc[k])


def shape_pointers(doc, pointers: list[str]):
    doc = copy.deepcopy(doc)
    for p in pointers:
        parts = [x.replace("~1", "/").replace("~0", "~") for x in p.split("/")[1:]]
        if parts:
            _shape(doc, parts)
    return doc


def _first_json_difference(a, b, path="") -> str | None:
    if type(a) is not type(b):
        return f"{path or '/'}: {json.dumps(a)[:120]} != {json.dumps(b)[:120]}"
    if isinstance(a, dict):
        for k in list(a.keys()) + [k for k in b.keys() if k not in a]:
            if k not in a or k not in b:
                return f"{path}/{k}: only in {'C#' if k in a else 'Rust'}"
            d = _first_json_difference(a[k], b[k], f"{path}/{k}")
            if d:
                return d
        if list(a.keys()) != list(b.keys()):
            return f"{path or '/'}: key order {list(a.keys())} != {list(b.keys())}"
        return None
    if isinstance(a, list):
        if len(a) != len(b):
            return f"{path or '/'}: {len(a)} items != {len(b)} items"
        for i, (x, y) in enumerate(zip(a, b)):
            d = _first_json_difference(x, y, f"{path}/{i}")
            if d:
                return d
        return None
    return None if a == b else f"{path or '/'}: {json.dumps(a)[:120]} != {json.dumps(b)[:120]}"


def diff(entry: dict, a: Response, b: Response) -> list[str]:
    """Differences between the C# answer (a) and the Rust answer (b), each prefixed with the request's name."""
    name = entry.get("name", entry.get("path", "?"))
    problems = []
    if a.status != b.status:
        problems.append(f"{name}: status {a.status} != {b.status}")
    for h in CONTRACT_HEADERS:
        if a.headers.get(h) != b.headers.get(h):
            problems.append(f"{name}: header {h}: {a.headers.get(h)!r} != {b.headers.get(h)!r}")
    mode = entry.get("compare", "json")
    if mode == "bytes" and a.body != b.body:
        problems.append(f"{name}: body bytes differ ({len(a.body)} vs {len(b.body)} bytes)")
    elif mode == "json":
        try:
            ja, jb = json.loads(a.body or b"null"), json.loads(b.body or b"null")
        except ValueError as e:
            problems.append(f"{name}: body is not JSON ({e})")
        else:
            ignore, shape = entry.get("ignore", []), entry.get("shape", [])
            d = _first_json_difference(shape_pointers(strip_pointers(ja, ignore), shape),
                                       shape_pointers(strip_pointers(jb, ignore), shape))
            if d:
                problems.append(f"{name}: {d}")
    return problems


# ---------------------------------------------------------------- running agents
def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def request(port: int, entry: dict, extra_headers: dict | None = None, lan_port: int | None = None) -> Response:
    values = {"port": port, "lanport": lan_port}
    wifi = bool(entry.get("wifi"))
    target = lan_port if wifi else port
    headers = {"Host": f"127.0.0.1:{target}"}
    headers.update({k: fill(v, values) for k, v in (entry.get("headers") or {}).items()})
    headers.update(extra_headers or {})
    body = entry.get("body")
    data = None if body is None else fill(body, values).encode()
    if wifi:
        # the phone pins the certificate's fingerprint; here both agents' fingerprints are compared instead
        context = ssl.create_default_context()
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
        conn = http.client.HTTPSConnection("127.0.0.1", target, timeout=15, context=context)
    else:
        conn = http.client.HTTPConnection("127.0.0.1", target, timeout=15)
    try:
        conn.putrequest(entry.get("method", "GET"), fill(entry["path"], values), skip_host=True, skip_accept_encoding=True)
        for k, v in headers.items():
            conn.putheader(k, v)
        if data is not None:
            conn.putheader("Content-Length", str(len(data)))
        conn.endheaders(data)
        r = conn.getresponse()
        hdrs = {k.lower(): v for k, v in r.getheaders()}
        hdrs.pop("date", None)
        if wifi:
            hdrs["x-peer-fingerprint"] = hashlib.sha256(conn.sock.getpeercert(binary_form=True)).hexdigest()
        if "x-config-version" in hdrs:
            hdrs["x-config-version-present"] = "yes"
        return Response(r.status, hdrs, r.read())
    finally:
        conn.close()


class Agent:
    def __init__(self, label: str, cmd: str, fixture: Path, cert: Path):
        self.label = label
        self.dir = Path(tempfile.mkdtemp(prefix=f"parity-{label}-"))
        self.config = self.dir / "config.json"
        self.port = free_port()
        self.lan_port = free_port()
        text = fixture.read_text(encoding="utf-8")
        if wifi_enabled(text):
            self.config.write_text(prepare_config(text, self.lan_port), encoding="utf-8")
            shutil.copyfile(cert, self.dir / "lan-cert.pfx")
        else:
            shutil.copyfile(fixture, self.config)
        self.log = open(self.dir / "stdout.log", "wb")
        argv = shlex.split(cmd) + ["--config", str(self.config), "--port", str(self.port)]
        env = dict(os.environ, HOME=str(self.dir), XDG_CONFIG_HOME=str(self.dir), XDG_DATA_HOME=str(self.dir))
        self.proc = subprocess.Popen(argv, stdout=self.log, stderr=subprocess.STDOUT, env=env)

    def wait_ready(self, timeout=60.0):
        end = time.time() + timeout
        while time.time() < end:
            if self.proc.poll() is not None:
                raise RuntimeError(f"{self.label} agent exited with {self.proc.returncode}; see {self.dir}/stdout.log")
            try:
                if request(self.port, {"path": "/api/info"}).status == 200:
                    return
            except OSError:
                pass
            time.sleep(0.3)
        raise RuntimeError(f"{self.label} agent did not answer on port {self.port} within {timeout:.0f} s")

    def config_version(self) -> str:
        r = request(self.port, {"path": "/api/settings"})
        return r.headers.get("x-config-version", "")

    def stop(self):
        if self.proc.poll() is None:
            self.proc.terminate()  # by PID: only the process this harness started
            try:
                self.proc.wait(10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.log.close()


def run(csharp_cmd: str, rust_cmd: str, corpus: list[dict], fixture: Path, cert: Path, keep: bool) -> list[str]:
    agents = [Agent("csharp", csharp_cmd, fixture, cert), Agent("rust", rust_cmd, fixture, cert)]
    problems: list[str] = []
    finished = False
    try:
        for a in agents:
            a.wait_ready()
        for entry in corpus:
            if entry.get("wait"):
                time.sleep(float(entry["wait"]))
            answers = []
            for a in agents:
                extra = {"X-Config-Version": a.config_version()} if entry.get("config_version") else None
                answers.append(request(a.port, entry, extra, a.lan_port))
            problems += diff(entry, answers[0], answers[1])
            if entry.get("config_file"):
                ca, cb = agents[0].config.read_bytes(), agents[1].config.read_bytes()
                if ca != cb:
                    problems.append(f"{entry.get('name')}: config.json differs after the request "
                                    f"({agents[0].config} vs {agents[1].config})")
        finished = True
    finally:
        for a in agents:
            a.stop()
        # a failed start or a difference keeps both folders (logs, configs) for a look
        if not keep and finished and not problems:
            for a in agents:
                shutil.rmtree(a.dir, ignore_errors=True)
        else:
            print("agent folders kept: " + ", ".join(str(a.dir) for a in agents), file=sys.stderr)
    return problems


def main() -> int:
    here = Path(__file__).resolve().parent
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--csharp", required=True, help="command that starts the C# agent")
    ap.add_argument("--rust", required=True, help="command that starts the Rust agent")
    ap.add_argument("--corpus", required=True, type=Path)
    ap.add_argument("--fixture", type=Path, default=here / "fixtures" / "default.json")
    ap.add_argument("--fixture-cert", type=Path, default=here / "fixtures" / "lan-cert.pfx",
                    help="the Wi-Fi certificate both agents get when the fixture turns Wi-Fi on")
    ap.add_argument("--keep", action="store_true", help="keep the agents' temporary folders")
    args = ap.parse_args()
    corpus = json.loads(args.corpus.read_text(encoding="utf-8"))
    problems = run(args.csharp, args.rust, corpus, args.fixture, args.fixture_cert, args.keep)
    for p in problems:
        print(p)
    print(f"{len(corpus)} requests, {len(problems)} differences")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
