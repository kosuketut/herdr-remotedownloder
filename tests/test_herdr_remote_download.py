import hashlib
import http.client
import json
import os
from pathlib import Path
import socket
import socketserver
import tempfile
import threading
import unittest
from unittest import mock

import herdr_remote_download as download


class ResolveRemotePathTests(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)
        self.file = self.root / "result data.txt"
        self.file.write_text("result", encoding="utf-8")

    def tearDown(self):
        self.temp_dir.cleanup()

    def test_resolves_selected_absolute_path_with_line_suffix(self):
        context = {
            "selected_text": f"{self.file}:42:7",
            "focused_pane_cwd": str(self.root),
        }

        self.assertEqual(download.resolve_path_from_context(context), self.file.resolve())

    def test_resolves_markdown_link(self):
        context = {
            "selected_text": f"[result]({self.file}:12)",
            "focused_pane_cwd": str(self.root),
        }

        self.assertEqual(download.resolve_path_from_context(context), self.file.resolve())

    def test_resolves_relative_path_from_focused_pane(self):
        context = {
            "selected_text": self.file.name,
            "focused_pane_cwd": str(self.root),
        }

        self.assertEqual(download.resolve_path_from_context(context), self.file.resolve())

    def test_prefers_clicked_file_url(self):
        context = {
            "selected_text": "not-the-file.txt",
            "clicked_url": self.file.as_uri(),
            "focused_pane_cwd": str(self.root),
        }

        self.assertEqual(download.resolve_path_from_context(context), self.file.resolve())

    def test_rejects_multiline_selection(self):
        context = {
            "selected_text": f"{self.file}\n{self.file}",
            "focused_pane_cwd": str(self.root),
        }

        with self.assertRaisesRegex(download.DownloadError, "one file path"):
            download.resolve_path_from_context(context)


class ReceiverTests(unittest.TestCase):
    TOKEN = "ab" * 32

    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)
        self.download_dir = self.root / "downloads"
        self.server = download.DownloadHTTPServer(
            ("127.0.0.1", 0),
            destination=self.download_dir,
            token=self.TOKEN,
            max_bytes=1024 * 1024,
            verbose=False,
        )
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)
        self.temp_dir.cleanup()

    @property
    def port(self):
        return self.server.server_address[1]

    def test_upload_saves_file_without_overwriting_existing_name(self):
        self.download_dir.mkdir()
        existing = self.download_dir / "report.txt"
        existing.write_text("keep", encoding="utf-8")
        source = self.root / "report.txt"
        source.write_bytes(b"new report")

        result = download.upload_file(
            source,
            host="127.0.0.1",
            port=self.port,
            token=self.TOKEN,
            timeout=5,
        )

        saved = Path(result["path"])
        self.assertEqual(saved.name, "report (1).txt")
        self.assertEqual(saved.read_bytes(), b"new report")
        self.assertEqual(existing.read_text(encoding="utf-8"), "keep")
        self.assertEqual(result["sha256"], hashlib.sha256(b"new report").hexdigest())

    def test_rejects_invalid_token(self):
        source = self.root / "secret.txt"
        source.write_text("secret", encoding="utf-8")

        with self.assertRaisesRegex(download.DownloadError, "401"):
            download.upload_file(
                source,
                host="127.0.0.1",
                port=self.port,
                token="cd" * 32,
                timeout=5,
            )

        self.assertFalse(self.download_dir.exists())

    def test_unavailable_receiver_fails_before_hashing_file(self):
        source = self.root / "report.txt"
        source.write_text("report", encoding="utf-8")
        probe = socket.socket()
        probe.bind(("127.0.0.1", 0))
        unavailable_port = probe.getsockname()[1]
        probe.close()

        with mock.patch.object(download, "sha256_file") as checksum:
            with self.assertRaisesRegex(download.DownloadError, "receiver is unavailable"):
                download.upload_file(
                    source,
                    host="127.0.0.1",
                    port=unavailable_port,
                    token=self.TOKEN,
                    timeout=1,
                )

        checksum.assert_not_called()

    def test_upload_through_unix_socket(self):
        source = self.root / "unix report.txt"
        source.write_bytes(b"unix socket transfer")
        socket_path = self.root / "receiver.sock"
        unix_server = socketserver.UnixStreamServer(
            str(socket_path), download.DownloadRequestHandler
        )
        unix_server.destination = self.download_dir
        unix_server.token = self.TOKEN
        unix_server.max_bytes = 1024 * 1024
        unix_server.verbose = False
        thread = threading.Thread(target=unix_server.serve_forever, daemon=True)
        thread.start()
        try:
            result = download.upload_file(
                source,
                host="127.0.0.1",
                port=0,
                token=self.TOKEN,
                timeout=5,
                socket_path=socket_path,
            )
        finally:
            unix_server.shutdown()
            unix_server.server_close()
            thread.join(timeout=2)

        saved = Path(result["path"])
        self.assertEqual(saved.name, source.name)
        self.assertEqual(saved.read_bytes(), b"unix socket transfer")

    def test_rejects_checksum_mismatch_and_removes_partial_file(self):
        body = b"corrupted"
        filename = download.encode_header_value("result.bin")
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
        connection.request(
            "POST",
            "/v1/files",
            body=body,
            headers={
                "Authorization": f"Bearer {self.TOKEN}",
                "Content-Length": str(len(body)),
                "X-Herdr-Filename": filename,
                "X-Herdr-SHA256": "0" * 64,
            },
        )
        response = connection.getresponse()
        response.read()
        connection.close()

        self.assertEqual(response.status, 422)
        self.assertEqual(list(self.download_dir.glob("*")), [])

    def test_health_endpoint(self):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
        connection.request("GET", "/health")
        response = connection.getresponse()
        payload = json.loads(response.read())
        connection.close()

        self.assertEqual(response.status, 200)
        self.assertEqual(payload["status"], "ok")


class TokenTests(unittest.TestCase):
    def test_ensure_token_creates_private_reusable_token(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            token_path = Path(temp_dir) / "config" / "token"

            first = download.ensure_token(token_path)
            second = download.ensure_token(token_path)

            self.assertEqual(first, second)
            self.assertEqual(len(first), 64)
            if os.name == "posix":
                self.assertEqual(token_path.stat().st_mode & 0o777, 0o600)


class LaunchdTests(unittest.TestCase):
    def test_launchd_service_binds_only_to_loopback(self):
        plist = download._launchd_plist(
            python_path=Path("/usr/bin/python3"),
            script_path=Path("/tmp/herdr_remote_download.py"),
            token_path=Path("/tmp/token"),
            download_dir=Path("/tmp/downloads"),
            port=18340,
            log_path=Path("/tmp/download.log"),
        )

        arguments = plist["ProgramArguments"]
        self.assertEqual(arguments[arguments.index("--host") + 1], "127.0.0.1")
        self.assertEqual(arguments[arguments.index("--port") + 1], "18340")
        self.assertTrue(plist["RunAtLoad"])
        self.assertTrue(plist["KeepAlive"])


class KeybindingConfigTests(unittest.TestCase):
    def test_adds_plugin_keybinding_before_worktrees_and_is_idempotent(self):
        original = '[keys]\nprefix = "ctrl+b"\n\n[worktrees]\ndirectory = "~/.herdr/worktrees"\n'

        updated, changed = download.add_keybinding_config(original, "prefix+d")
        repeated, changed_again = download.add_keybinding_config(updated, "prefix+d")

        self.assertTrue(changed)
        self.assertFalse(changed_again)
        self.assertEqual(repeated, updated)
        self.assertLess(
            updated.index(download.PLUGIN_ACTION),
            updated.index("[worktrees]"),
        )

    def test_rejects_existing_keybinding(self):
        original = '[[keys.command]]\nkey = "prefix+d"\ncommand = "other"\n'

        with self.assertRaisesRegex(download.DownloadError, "already in use"):
            download.add_keybinding_config(original, "prefix+d")

    def test_upgrades_existing_direct_download_keybinding_to_picker(self):
        original = (
            '[[keys.command]]\n'
            'key = "prefix+d"\n'
            'type = "plugin_action"\n'
            f'command = "{download.DOWNLOAD_ACTION}"\n'
            'description = "download selected remote file"\n'
        )

        updated, changed = download.add_keybinding_config(original, "prefix+d")

        self.assertTrue(changed)
        self.assertIn(f'command = "{download.PICK_ACTION}"', updated)
        self.assertIn(
            f'description = "{download.PICK_DESCRIPTION}"',
            updated,
        )
        self.assertNotIn(f'command = "{download.DOWNLOAD_ACTION}"', updated)


if __name__ == "__main__":
    unittest.main()
