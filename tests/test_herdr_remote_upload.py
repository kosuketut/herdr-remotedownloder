import base64
import hashlib
import io
import json
import os
import socket
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock

import herdr_remote_upload as upload


TOKEN = "ab" * 32


def read_request(connection):
    request = bytearray()
    while b"\r\n\r\n" not in request:
        chunk = connection.recv(4096)
        if not chunk:
            break
        request.extend(chunk)
    return bytes(request)


def file_response(name, body, checksum=None):
    encoded = base64.urlsafe_b64encode(name.encode()).decode().rstrip("=")
    checksum = checksum or hashlib.sha256(body).hexdigest()
    return (
        "HTTP/1.1 200 OK\r\n"
        f"Content-Length: {len(body)}\r\n"
        f"X-Herdr-Filename: {encoded}\r\n"
        f"X-Herdr-SHA256: {checksum}\r\n"
        "Connection: close\r\n\r\n"
    ).encode() + body


def batch_response(files):
    parts = []
    for name, body in files:
        encoded = base64.urlsafe_b64encode(name.encode()).decode().rstrip("=")
        parts.append(
            (
                f"Content-Length: {len(body)}\r\n"
                f"X-Herdr-Filename: {encoded}\r\n"
                f"X-Herdr-SHA256: {hashlib.sha256(body).hexdigest()}\r\n\r\n"
            ).encode()
            + body
        )
    payload = b"".join(parts)
    return (
        "HTTP/1.1 200 OK\r\n"
        f"Content-Type: {upload.BATCH_CONTENT_TYPE}\r\n"
        f"Content-Length: {len(payload)}\r\n"
        f"X-Herdr-File-Count: {len(files)}\r\n"
        "Connection: close\r\n\r\n"
    ).encode() + payload


class RemoteUploadTests(unittest.TestCase):
    def serve_response(self, connection, response, requests):
        requests.append(read_request(connection))
        connection.sendall(response)
        connection.close()

    def test_upload_verifies_content_and_avoids_overwrite(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary)
            (destination / "report.txt").write_text("keep", encoding="utf-8")
            server, client = socket.socketpair()
            requests = []
            thread = threading.Thread(
                target=self.serve_response,
                args=(server, file_response("report.txt", b"new report"), requests),
            )
            thread.start()
            saved = upload.receive_to_directory(
                client, destination, TOKEN, upload.DEFAULT_MAX_BYTES
            )[0]
            client.close()
            thread.join(timeout=2)
            self.assertEqual(saved.name, "report (1).txt")
            self.assertEqual(saved.read_bytes(), b"new report")
            self.assertEqual((destination / "report.txt").read_text(), "keep")
            self.assertIn(b"POST /v1/choose-file HTTP/1.1", requests[0])
            self.assertIn(f"Authorization: Bearer {TOKEN}".encode(), requests[0])

    def test_upload_receives_multiple_selected_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary)
            server, client = socket.socketpair()
            thread = threading.Thread(
                target=self.serve_response,
                args=(
                    server,
                    batch_response(
                        [("first.txt", b"first"), ("second.bin", b"second")]
                    ),
                    [],
                ),
            )
            thread.start()
            saved = upload.receive_to_directory(
                client, destination, TOKEN, upload.DEFAULT_MAX_BYTES
            )
            client.close()
            thread.join(timeout=2)
            self.assertEqual([path.name for path in saved], ["first.txt", "second.bin"])
            self.assertEqual(saved[0].read_bytes(), b"first")
            self.assertEqual(saved[1].read_bytes(), b"second")

    def test_upload_reports_progress_for_each_selected_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary)
            server, client = socket.socketpair()
            thread = threading.Thread(
                target=self.serve_response,
                args=(
                    server,
                    batch_response(
                        [("first.txt", b"first"), ("second.bin", b"second")]
                    ),
                    [],
                ),
            )
            thread.start()
            output = io.StringIO()
            with mock.patch("sys.stdout", output):
                upload.receive_to_directory(
                    client,
                    destination,
                    TOKEN,
                    upload.DEFAULT_MAX_BYTES,
                    progress=True,
                )
            client.close()
            thread.join(timeout=2)
            text = output.getvalue()
            self.assertIn("Preparing selected files on Mac...", text)
            self.assertIn("Receiving 1/2: first.txt 100%", text)
            self.assertIn("Receiving 2/2: second.bin 100%", text)

    def test_checksum_failure_removes_partial_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary)
            server, client = socket.socketpair()
            thread = threading.Thread(
                target=self.serve_response,
                args=(server, file_response("broken.bin", b"bad", "0" * 64), []),
            )
            thread.start()
            with self.assertRaisesRegex(upload.UploadError, "SHA-256"):
                upload.receive_to_directory(
                    client, destination, TOKEN, upload.DEFAULT_MAX_BYTES
                )
            client.close()
            thread.join(timeout=2)
            self.assertEqual(list(destination.iterdir()), [])

    def test_context_prefers_focused_pane_directory(self):
        context = {
            "focused_pane_cwd": "/tmp/focused",
            "workspace_cwd": "/tmp/workspace",
        }
        with mock.patch.dict(
            os.environ, {"HERDR_PLUGIN_CONTEXT_JSON": json.dumps(context)}
        ):
            self.assertEqual(upload.destination_from_context(), Path("/tmp/focused"))

    def test_legacy_keybinding_is_migrated_and_idempotent(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.toml"
            original = (
                '[[keys.command]]\nkey = "prefix+u"\ntype = "plugin_action"\n'
                f'command = "{upload.LEGACY_UPLOAD_ACTION}"\n'
                f'description = "{upload.UPLOAD_DESCRIPTION}"\n'
            )
            path.write_text(original, encoding="utf-8")
            self.assertTrue(upload.configure_upload_keybinding(path, "prefix+u"))
            updated = path.read_text(encoding="utf-8")
            self.assertIn(upload.UPLOAD_ACTION, updated)
            self.assertNotIn(upload.LEGACY_UPLOAD_ACTION, updated)
            self.assertFalse(upload.configure_upload_keybinding(path, "prefix+u"))
            self.assertEqual(path.read_text(encoding="utf-8"), updated)


if __name__ == "__main__":
    unittest.main()
