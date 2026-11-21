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

    def test_shape_pointers_compare_keys_and_kinds_only(self):
        entry = {"name": "state", "compare": "json", "shape": ["/pc", "/ping"]}
        a = resp(body=b'{"pc": {"cpu": 3.5, "disks": [{"name": "C:", "free": 1}], "gpus": []}, "ping": {"ms": null}, "x": 1}')
        b = resp(body=b'{"pc": {"cpu": 9, "disks": [{"name": "D:", "free": 2}], "gpus": []}, "ping": {"ms": 12}, "x": 1}')
        self.assertEqual(parity.diff(entry, a, b), [])

    def test_shape_pointers_still_report_keys_order_and_lengths(self):
        entry = {"name": "state", "compare": "json", "shape": ["/pc"]}
        base = b'{"pc": {"cpu": 1, "disks": [{"name": "C:"}]}}'
        missing = b'{"pc": {"disks": [{"name": "C:"}]}}'
        reordered = b'{"pc": {"disks": [{"name": "C:"}], "cpu": 1}}'
        longer = b'{"pc": {"cpu": 1, "disks": [{"name": "C:"}, {"name": "D:"}]}}'
        null_pc = b'{"pc": null}'
        for other in (missing, reordered, longer, null_pc):
            self.assertEqual(len(parity.diff(entry, resp(body=base), resp(body=other))), 1, other)
        # outside the shape pointers values still count
        self.assertEqual(len(parity.diff(entry, resp(body=b'{"pc": 1, "x": 1}'), resp(body=b'{"pc": 2, "x": 2}'))), 1)

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


    def test_placeholders_use_each_agents_wifi_port(self):
        self.assertEqual(parity.fill("{port}/{lanport}", {"port": 1, "lanport": 2}), "1/2")

    def test_shape_pointers_hide_scalar_values_too(self):
        entry = {"name": "pair start", "compare": "json", "shape": ["/link", "/expiresAt"]}
        a = resp(body=b'{"link": "https://a", "expiresAt": 1, "attempt": 1}')
        b = resp(body=b'{"link": "https://b", "expiresAt": 2, "attempt": 1}')
        self.assertEqual(parity.diff(entry, a, b), [])
        # the keys and the values outside the pointers still count
        self.assertEqual(len(parity.diff(entry, a, resp(body=b'{"expiresAt": 2, "attempt": 1}'))), 1)
        self.assertEqual(len(parity.diff(entry, a, resp(body=b'{"link": "x", "expiresAt": 2, "attempt": 2}'))), 1)


class ConfigTests(unittest.TestCase):
    def test_a_wifi_fixture_gets_each_agents_wifi_port(self):
        text = '{\n  "lan": { "enabled": true, "port": 8766, "token": "t" },\n  "x": 1\n}\n'
        out = parity.prepare_config(text, 19000)
        self.assertEqual(parity.json.loads(out)["lan"], {"enabled": True, "port": 19000, "token": "t"})
        self.assertEqual(parity.json.loads(out)["x"], 1)
        self.assertTrue(parity.wifi_enabled(text))

    def test_a_fixture_without_wifi_is_copied_as_it_is(self):
        for text in ('{ "lan": { "enabled": false, "port": 8766 } }', '{"deviceSerial": "x"}'):
            self.assertEqual(parity.prepare_config(text, 19000), text)
            self.assertFalse(parity.wifi_enabled(text))


if __name__ == "__main__":
    unittest.main()
