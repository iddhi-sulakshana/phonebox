"""Tests for the parity harness's comparison logic (pure functions; no agents are started)."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import parity  # noqa: E402


def resp(status=200, body=b"{}", headers=None):
    return parity.Response(status, {k.lower(): v for k, v in (headers or {"content-type": "application/json"}).items()}, body)


class DiffTests(unittest.TestCase):
    def test_diff_ignores_pointers(self):
        entry = {"name": "state", "compare": "json", "ignore": ["/ts"]}
        a = resp(body=b'{"ts": 1, "host": "x"}')
        b = resp(body=b'{"ts": 2, "host": "x"}')
        self.assertEqual(parity.diff(entry, a, b), [])

    def test_diff_ignores_wildcard_pointers(self):
        entry = {"name": "phones", "compare": "json", "ignore": ["/phones/*/lastSeen"]}
        a = resp(body=b'{"phones": [{"id": "a", "lastSeen": 1}]}')
        b = resp(body=b'{"phones": [{"id": "a", "lastSeen": 9}]}')
        self.assertEqual(parity.diff(entry, a, b), [])

    def test_diff_reports_json_value(self):
        entry = {"name": "info", "compare": "json", "ignore": []}
        problems = parity.diff(entry, resp(body=b'{"os": "linux"}'), resp(body=b'{"os": "macos"}'))
        self.assertEqual(len(problems), 1)
        self.assertIn("info", problems[0])
        self.assertIn("/os", problems[0])

    def test_diff_reports_status(self):
        entry = {"name": "guard", "compare": "status"}
        problems = parity.diff(entry, resp(status=200), resp(status=403))
        self.assertEqual(len(problems), 1)
        self.assertIn("guard", problems[0])
        self.assertIn("200", problems[0])
        self.assertIn("403", problems[0])

    def test_bytes_mode(self):
        entry = {"name": "panel", "compare": "bytes"}
        self.assertEqual(parity.diff(entry, resp(body=b"abc"), resp(body=b"abc")), [])
        self.assertEqual(len(parity.diff(entry, resp(body=b"abc"), resp(body=b"abd"))), 1)

    def test_compared_headers_must_match(self):
        entry = {"name": "frame", "compare": "status"}
        a = resp(headers={"x-frame-options": "DENY"})
        b = resp(headers={"x-frame-options": "SAMEORIGIN"})
        self.assertEqual(len(parity.diff(entry, a, b)), 1)

    def test_placeholders_use_each_agents_port(self):
        self.assertEqual(parity.fill("http://127.0.0.1:{port}", {"port": 18765}), "http://127.0.0.1:18765")


if __name__ == "__main__":
    unittest.main()
