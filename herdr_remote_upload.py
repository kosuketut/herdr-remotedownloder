#!/usr/bin/env python3
"""Receive a file chosen by the connected Mac service into a remote Herdr pane."""

from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import json
import os
import shutil
import socket
import sys
from pathlib import Path
from typing import Any, BinaryIO, Dict, Optional, Tuple


PLUGIN_ID = "kosukeyano.remote-download"
UPLOAD_ACTION = f"{PLUGIN_ID}.upload"
LEGACY_UPLOAD_ACTION = "kosukeyano.remote-upload.choose"
UPLOAD_DESCRIPTION = "upload a file from the connected Mac"
DEFAULT_KEY = "prefix+u"
CHOOSE_FILE_PATH = "/v1/choose-file"
DEFAULT_MAX_BYTES = 512 * 1024 * 1024
DEFAULT_TIMEOUT_SECONDS = 600
MAX_HEADER_BYTES = 64 * 1024
MAX_ERROR_BYTES = 64 * 1024
COPY_BUFFER_BYTES = 1024 * 1024


class UploadError(Exception):
    """An expected transfer or configuration error."""


def home_dir() -> Path:
    value = os.environ.get("HOME")
    if not value:
        raise UploadError("HOME is not set")
    return Path(value)


def default_token_path() -> Path:
    return home_dir() / ".config/herdr-remote-download/token"


def plugin_token_path() -> Path:
    override = os.environ.get("HERDR_DOWNLOAD_TOKEN_FILE")
    if override:
        return Path(override).expanduser()
    plugin_config = os.environ.get("HERDR_PLUGIN_CONFIG_DIR")
    if plugin_config:
        candidate = Path(plugin_config) / "token"
        if candidate.exists():
            return candidate
    return default_token_path()


def default_herdr_config_path() -> Path:
    return home_dir() / ".config/herdr/config.toml"


def remote_user() -> str:
    value = os.environ.get("USER") or os.environ.get("LOGNAME")
    if not value or "/" in value or "\x00" in value:
        raise UploadError("could not determine a safe remote user name")
    return value


def default_remote_socket() -> Path:
    override = os.environ.get("HERDR_DOWNLOAD_SOCKET")
    if override:
        return Path(override).expanduser()
    return Path("/tmp") / f"herdr-remote-download-{remote_user()}.sock"


def validate_token(value: str) -> str:
    token = value.strip().lower()
    if len(token) != 64 or any(character not in "0123456789abcdef" for character in token):
        raise UploadError("token must contain exactly 64 hexadecimal characters")
    return token


def read_token(path: Path) -> str:
    try:
        return validate_token(path.read_text(encoding="ascii"))
    except OSError as error:
        raise UploadError(f"cannot read token file {path}: {error}") from error


def destination_from_context() -> Path:
    raw = os.environ.get("HERDR_PLUGIN_CONTEXT_JSON")
    if not raw:
        raise UploadError("HERDR_PLUGIN_CONTEXT_JSON is not set")
    try:
        context = json.loads(raw)
    except json.JSONDecodeError as error:
        raise UploadError("HERDR_PLUGIN_CONTEXT_JSON is invalid") from error
    for key in ("focused_pane_cwd", "workspace_cwd"):
        value = context.get(key)
        if isinstance(value, str) and value:
            return Path(value).expanduser()
    raise UploadError("the Herdr plugin context did not include a destination directory")


def read_http_headers(reader: BinaryIO) -> Tuple[int, Dict[str, str]]:
    status_line = reader.readline(MAX_HEADER_BYTES + 1)
    if not status_line or len(status_line) > MAX_HEADER_BYTES:
        raise UploadError("the Mac service returned an invalid HTTP status line")
    try:
        parts = status_line.decode("ascii").split()
        status = int(parts[1])
    except (UnicodeDecodeError, IndexError, ValueError) as error:
        raise UploadError("the Mac service returned an invalid HTTP status line") from error

    headers: Dict[str, str] = {}
    total = len(status_line)
    while True:
        line = reader.readline(MAX_HEADER_BYTES + 1)
        total += len(line)
        if not line or len(line) > MAX_HEADER_BYTES or total > MAX_HEADER_BYTES:
            raise UploadError("the Mac service returned invalid HTTP headers")
        if line in {b"\r\n", b"\n"}:
            return status, headers
        try:
            name, value = line.decode("utf-8").split(":", 1)
        except (UnicodeDecodeError, ValueError) as error:
            raise UploadError("the Mac service returned an invalid HTTP header") from error
        headers[name.strip().lower()] = value.strip()


def decode_filename(value: Optional[str]) -> str:
    if not value or len(value) > 1024:
        raise UploadError("the Mac service did not provide a valid filename")
    try:
        padding = "=" * (-len(value) % 4)
        name = base64.urlsafe_b64decode(value + padding).decode("utf-8")
    except (ValueError, UnicodeDecodeError) as error:
        raise UploadError("the Mac service provided an invalid filename") from error
    if (
        not name
        or name in {".", ".."}
        or Path(name).name != name
        or len(name.encode("utf-8")) > 240
        or any(ord(character) < 32 or ord(character) == 127 for character in name)
    ):
        raise UploadError("the Mac service provided an unsafe filename")
    return name


def create_unique_file(directory: Path, name: str) -> Tuple[Path, int]:
    if not directory.is_dir():
        raise UploadError(f"destination directory does not exist: {directory}")
    stem, separator, suffix = name.rpartition(".")
    if not separator or not stem:
        stem, suffix = name, ""
    else:
        suffix = "." + suffix
    for index in range(10000):
        candidate_name = name if index == 0 else f"{stem} ({index}){suffix}"
        candidate = directory / candidate_name
        try:
            descriptor = os.open(
                str(candidate), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600
            )
            return candidate, descriptor
        except FileExistsError:
            continue
    raise UploadError("could not allocate a unique destination filename")


def receive_to_directory(
    connection: socket.socket,
    destination: Path,
    token: str,
    max_bytes: int,
) -> Path:
    request = (
        f"POST {CHOOSE_FILE_PATH} HTTP/1.1\r\n"
        "Host: localhost\r\n"
        f"Authorization: Bearer {validate_token(token)}\r\n"
        "Content-Length: 0\r\n"
        "Connection: close\r\n\r\n"
    )
    connection.sendall(request.encode("ascii"))
    with connection.makefile("rb") as reader:
        status, headers = read_http_headers(reader)
        try:
            length = int(headers.get("content-length", ""))
        except ValueError as error:
            raise UploadError("the Mac service returned an invalid Content-Length") from error
        if length < 0:
            raise UploadError("the Mac service returned an invalid Content-Length")
        if status != 200:
            if length > MAX_ERROR_BYTES:
                raise UploadError(f"the Mac service returned HTTP {status}")
            body = reader.read(length)
            try:
                detail = json.loads(body).get("error")
            except (UnicodeDecodeError, json.JSONDecodeError, AttributeError):
                detail = None
            suffix = f": {detail}" if detail else ""
            raise UploadError(f"the Mac service returned HTTP {status}{suffix}")
        if length > max_bytes:
            raise UploadError(f"the selected file exceeds the {max_bytes} byte limit")

        name = decode_filename(headers.get("x-herdr-filename"))
        expected_digest = headers.get("x-herdr-sha256", "")
        if len(expected_digest) != 64 or any(
            character not in "0123456789abcdef" for character in expected_digest
        ):
            raise UploadError("the Mac service returned an invalid SHA-256 digest")

        path, descriptor = create_unique_file(destination, name)
        digest = hashlib.sha256()
        remaining = length
        try:
            with os.fdopen(descriptor, "wb") as stream:
                while remaining:
                    chunk = reader.read(min(COPY_BUFFER_BYTES, remaining))
                    if not chunk:
                        raise UploadError("the Mac service closed before the file was complete")
                    stream.write(chunk)
                    digest.update(chunk)
                    remaining -= len(chunk)
                stream.flush()
                os.fsync(stream.fileno())
            if not hmac.compare_digest(digest.hexdigest(), expected_digest):
                raise UploadError("SHA-256 verification failed")
        except Exception:
            path.unlink(missing_ok=True)
            raise
        return path


def receive_file(
    destination: Path,
    socket_path: Path,
    token_path: Path,
    timeout: int,
    max_bytes: int,
) -> Path:
    token = read_token(token_path)
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        connection.settimeout(3)
        connection.connect(str(socket_path))
        connection.settimeout(timeout)
        return receive_to_directory(connection, destination, token, max_bytes)
    except OSError as error:
        raise UploadError(
            f"cannot use the Mac transfer tunnel at {socket_path}: {error}; reconnect hr"
        ) from error
    finally:
        connection.close()


def configure_upload_keybinding(path: Path, key: str) -> bool:
    if not key or any(
        not (character.isascii() and (character.isalnum() or character in "+_-"))
        for character in key
    ):
        raise UploadError("the key must use only ASCII letters, numbers, '+', '_' or '-'")
    content = path.read_text(encoding="utf-8") if path.exists() else ""
    if UPLOAD_ACTION in content:
        return False
    if LEGACY_UPLOAD_ACTION in content:
        updated = content.replace(LEGACY_UPLOAD_ACTION, UPLOAD_ACTION, 1)
    else:
        if f'key = "{key}"' in content:
            raise UploadError(f"keybinding {key} is already in use")
        block = (
            "[[keys.command]]\n"
            f'key = "{key}"\n'
            'type = "plugin_action"\n'
            f'command = "{UPLOAD_ACTION}"\n'
            f'description = "{UPLOAD_DESCRIPTION}"\n'
        )
        updated = content.rstrip() + ("\n\n" if content.strip() else "") + block
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        shutil.copy2(path, path.with_name(f"{path.name}.before-remote-transfer"))
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    try:
        temporary.write_text(updated, encoding="utf-8")
        if path.exists():
            temporary.chmod(path.stat().st_mode & 0o777)
        os.replace(temporary, path)
    except Exception:
        temporary.unlink(missing_ok=True)
        raise
    return True


def bytes_from_megabytes(value: int) -> int:
    if value <= 0:
        raise UploadError("--max-mb must be greater than zero")
    return value * 1024 * 1024


def wait_for_close() -> None:
    try:
        input("\nPress Enter to close.")
    except (EOFError, KeyboardInterrupt):
        pass


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Upload a file chosen on the connected Mac to a remote Herdr pane"
    )
    subparsers = parser.add_subparsers(dest="command", required=True)
    for command, help_text in (
        ("upload-context", "upload into the focused Herdr pane directory"),
        ("upload", "upload into an explicit directory"),
    ):
        upload = subparsers.add_parser(command, help=help_text)
        if command == "upload":
            upload.add_argument("destination", type=Path)
        upload.add_argument("--socket", type=Path, default=default_remote_socket())
        upload.add_argument("--token-file", type=Path, default=plugin_token_path())
        upload.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT_SECONDS)
        upload.add_argument("--max-mb", type=int, default=DEFAULT_MAX_BYTES // (1024 * 1024))
        upload.add_argument("--interactive", action="store_true")

    configure = subparsers.add_parser(
        "configure-upload-keybinding", help="add or migrate the upload action keybinding"
    )
    configure.add_argument("--config", type=Path, default=default_herdr_config_path())
    configure.add_argument("--key", default=DEFAULT_KEY)
    return parser


def run(arguments: Optional[list[str]] = None) -> int:
    args = build_parser().parse_args(arguments)
    if args.command == "configure-upload-keybinding":
        changed = configure_upload_keybinding(args.config.expanduser(), args.key)
        print("updated" if changed else "already configured")
        return 0

    interactive = args.interactive
    if interactive:
        print("Choose a file in the dialog on your Mac.")
        print("The selected file will be saved in the focused pane's current directory.")
    destination = (
        destination_from_context() if args.command == "upload-context" else args.destination
    )
    try:
        path = receive_file(
            destination.expanduser(),
            args.socket,
            args.token_file.expanduser(),
            args.timeout,
            bytes_from_megabytes(args.max_mb),
        )
    except Exception:
        if interactive:
            print("\nUpload failed.", file=sys.stderr)
            wait_for_close()
        raise
    print(f"\nSaved: {path}")
    if interactive:
        wait_for_close()
    return 0


def main() -> int:
    try:
        return run()
    except (UploadError, OSError, ValueError) as error:
        print(f"herdr-remote-upload: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
