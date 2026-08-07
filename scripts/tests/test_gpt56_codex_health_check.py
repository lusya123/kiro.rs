import importlib.util
import json
import stat
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "gpt56_codex_health_check.py"
SPEC = importlib.util.spec_from_file_location("gpt56_codex_health_check", SCRIPT)
health = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = health
SPEC.loader.exec_module(health)


class ProbeHandler(BaseHTTPRequestHandler):
    last_payload = None

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        type(self).last_payload = json.loads(self.rfile.read(length))
        body = json.dumps(
            {
                "object": "response",
                "status": "completed",
                "model": "gpt-5.6-sol",
                "output": [
                    {
                        "type": "message",
                        "content": [{"type": "output_text", "text": health.SENTINEL}],
                    }
                ],
            }
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format, *_args):
        return


class HealthCheckTests(unittest.TestCase):
    def test_default_regex_targets_the_deployed_gpt_port_series(self):
        pattern = health.re.compile(health.DEFAULT_ACCOUNT_REGEX)
        for name in ["gpt-51892", "gpt-51899", "gpt-51900", "gpt-51999"]:
            self.assertIsNotNone(pattern.search(name), name)
        for name in ["gpt-51891", "gpt-52000", "pomoai-0.09-0.15", "kiro-51942"]:
            self.assertIsNone(pattern.search(name), name)

    def test_responses_url_is_normalized(self):
        self.assertEqual(health.responses_url("http://127.0.0.1:51942"), "http://127.0.0.1:51942/v1/responses")
        self.assertEqual(health.responses_url("http://127.0.0.1:51942/v1/"), "http://127.0.0.1:51942/v1/responses")

    def test_failure_categories_match_the_production_errors(self):
        self.assertEqual(
            health.classify_failure(400, "input item type 'reasoning' is not supported"),
            "protocol_reasoning_unsupported",
        )
        self.assertEqual(
            health.classify_failure(400, "`text.format.strict` is not supported"),
            "protocol_text_strict_unsupported",
        )
        self.assertEqual(
            health.classify_failure(502, "all credentials are disabled"),
            "credentials_unavailable",
        )

    def test_probe_sends_the_codex_compatibility_shape(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), ProbeHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            account = {
                "id": 42,
                "name": "gpt-51942",
                "status": "active",
                "schedulable": True,
                "credentials": {
                    "base_url": f"http://127.0.0.1:{server.server_port}",
                    "api_key": "test-secret",
                },
            }
            result = health.probe_account(account, "gpt-5.6-sol", 3)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)
        self.assertEqual(result["verdict"], "ok")
        payload = ProbeHandler.last_payload
        self.assertEqual(payload["input"][0]["type"], "reasoning")
        self.assertIs(payload["text"]["format"]["strict"], True)
        self.assertIs(payload["store"], False)
        self.assertIs(payload["stream"], True)

    def test_secret_bearing_inventory_must_be_private(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "accounts.json"
            path.write_text("[]", encoding="utf-8")
            path.chmod(stat.S_IRUSR | stat.S_IWUSR | stat.S_IRGRP)
            with self.assertRaises(RuntimeError):
                health.read_json_file(path, may_contain_secrets=True)


if __name__ == "__main__":
    unittest.main()
