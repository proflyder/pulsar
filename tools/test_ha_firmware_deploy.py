#!/usr/bin/env python3
"""Tests for tools/ha_firmware_deploy.py against a fake Home Assistant."""

from __future__ import annotations

import contextlib
import io
import json
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import ha_firmware_deploy as deployer  # noqa: E402

ENTITY = "update.station_firmware"
OLD = "v0.5.1000"
TAG = "v0.5.1001"


class FakeStation:
    """State machine of the HA update entity and the station restart."""

    def __init__(self, tag: str):
        self.tag = tag
        self.installed = OLD
        self.latest = OLD
        self.state = "on"
        self.update_calls = 0
        self.install_calls = 0
        self.polls = 0
        # Number of `homeassistant.update_entity` calls before the tag shows up.
        self.latest_after_updates = 0
        # State polls between `update.install` and the new version coming up.
        self.install_delay_polls = 1
        # Answer the install call without a response (the station went down).
        self.drop_install = False
        # Flip back to the old version this many polls after the install took.
        self.revert_after_polls = None
        self._install_at = None
        self._revert_at = None

    def snapshot(self) -> dict:
        return {
            "entity_id": ENTITY,
            "state": self.state,
            "attributes": {
                "installed_version": self.installed,
                "latest_version": self.latest,
            },
        }

    def poll(self) -> None:
        self.polls += 1
        if self._install_at is not None:
            if self.polls - self._install_at >= self.install_delay_polls:
                self.installed = self.tag
                self.state = "on"
                self._install_at = None
                if self.revert_after_polls is not None:
                    self._revert_at = self.polls + self.revert_after_polls
        if self._revert_at is not None and self.polls >= self._revert_at:
            self.installed = OLD
            self.state = "on"

    def check_updates(self) -> None:
        self.update_calls += 1
        if self.update_calls >= self.latest_after_updates:
            self.latest = self.tag

    def install(self) -> None:
        self.install_calls += 1
        self.state = "unavailable"
        self._install_at = self.polls


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    @property
    def station(self) -> FakeStation:
        return self.server.station  # type: ignore[attr-defined]

    def log_message(self, *args) -> None:  # keep the test output clean
        pass

    def _json(self, payload: dict, code: int = 200) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _authorized(self) -> bool:
        if self.headers.get("Authorization") != "Bearer test-token":
            self._json({"message": "unauthorized"}, 401)
            return False
        return True

    def do_GET(self) -> None:  # noqa: N802
        if not self._authorized():
            return
        if self.path != "/api/states/" + ENTITY:
            self._json({"message": "not found"}, 404)
            return
        self.station.poll()
        self._json(self.station.snapshot())

    def do_POST(self) -> None:  # noqa: N802
        if not self._authorized():
            return
        length = int(self.headers.get("Content-Length") or 0)
        payload = json.loads(self.rfile.read(length) or b"{}")
        if payload.get("entity_id") != ENTITY:
            self._json({"message": "wrong entity"}, 400)
            return
        if self.path == "/api/services/homeassistant/update_entity":
            self.station.check_updates()
            self._json([])
        elif self.path == "/api/services/update/install":
            self.station.install()
            if self.station.drop_install:
                # The station restarts inside the call: no response at all.
                self.close_connection = True
                return
            self._json([])
        else:
            self._json({"message": "not found"}, 404)


def start_server(station: FakeStation) -> ThreadingHTTPServer:
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
    server.daemon_threads = True
    server.station = station  # type: ignore[attr-defined]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def run_deploy(server: ThreadingHTTPServer, *extra: str) -> int:
    port = server.server_address[1]
    argv = [
        "--tag",
        TAG,
        "--ha-url",
        f"http://127.0.0.1:{port}",
        "--entity-id",
        ENTITY,
        "--token-file",
        str(_TOKEN_FILE),
        "--http-timeout",
        "2",
        "--poll-interval",
        "0.05",
        "--latest-timeout",
        "1.5",
        "--latest-retrigger",
        "0.2",
        "--install-timeout",
        "2",
        "--stability",
        "0.6",
        "--unavailable-tolerance",
        "0.6",
        *extra,
    ]
    # The token value must never appear on stdout/stderr.
    out = io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
        code = deployer.main(argv)
    captured = out.getvalue()
    assert "test-token" not in captured, "the tool printed the HA token"
    return code


_TOKEN_FILE = None
_TOKEN_DIR = None


def setUpModule() -> None:
    global _TOKEN_FILE, _TOKEN_DIR
    _TOKEN_DIR = tempfile.TemporaryDirectory(prefix="pulsar-deploy-test-")
    _TOKEN_FILE = Path(_TOKEN_DIR.name) / "ha_token"
    _TOKEN_FILE.write_text("test-token\n", encoding="utf-8")


def tearDownModule() -> None:
    _TOKEN_DIR.cleanup()


class DeployTests(unittest.TestCase):
    def setUp(self) -> None:
        self.station = FakeStation(TAG)
        self.server = start_server(self.station)

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()

    def test_already_installed(self) -> None:
        self.station.installed = TAG
        self.station.latest = TAG
        self.assertEqual(run_deploy(self.server), 0)
        self.assertEqual(self.station.install_calls, 0)
        self.assertEqual(self.station.update_calls, 0)

    def test_normal_install(self) -> None:
        self.station.latest_after_updates = 2
        self.station.install_delay_polls = 2
        self.assertEqual(run_deploy(self.server), 0)
        self.assertGreaterEqual(self.station.update_calls, 2)
        self.assertEqual(self.station.install_calls, 1)
        self.assertEqual(self.station.installed, TAG)

    def test_latest_never_appears(self) -> None:
        self.station.latest_after_updates = 10**9
        self.assertEqual(run_deploy(self.server), 1)
        self.assertEqual(self.station.install_calls, 0)
        self.assertGreaterEqual(self.station.update_calls, 2)

    def test_install_call_drops_the_connection(self) -> None:
        self.station.drop_install = True
        self.station.install_delay_polls = 2
        self.assertEqual(run_deploy(self.server), 0)
        self.assertEqual(self.station.install_calls, 1)
        self.assertEqual(self.station.installed, TAG)

    def test_rollback_after_install(self) -> None:
        self.station.install_delay_polls = 1
        self.station.revert_after_polls = 2
        self.assertEqual(run_deploy(self.server), 1)
        self.assertEqual(self.station.installed, OLD)


if __name__ == "__main__":
    unittest.main()
