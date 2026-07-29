#!/usr/bin/env python3
"""Transfer a selected remote file to the machine running a Herdr remote client."""

from __future__ import annotations

import argparse
import base64
import binascii
import getpass
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import plistlib
import re
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
from typing import Any, Optional
from urllib.parse import unquote, urlparse


PLUGIN_ID = "kosukeyano.remote-download"
DEFAULT_PORT = 18340
DEFAULT_MAX_BYTES = 512 * 1024 * 1024
DEFAULT_TIMEOUT_SECONDS = 300
PREFLIGHT_TIMEOUT_SECONDS = 3
TOKEN_HEX_LENGTH = 64
TRANSFER_PATH = "/v1/files"
LAUNCHD_LABEL = "com.kosukeyano.herdr-remote-download"
DOWNLOAD_ACTION = f"{PLUGIN_ID}.download"
PICK_ACTION = f"{PLUGIN_ID}.pick"
PLUGIN_ACTION = PICK_ACTION
PICK_DESCRIPTION = "pick a remote file to download"


class DownloadError(RuntimeError):
    """Expected user-facing transfer failure."""


def default_config_dir() -> Path:
    root = os.environ.get("XDG_CONFIG_HOME")
    if root:
        return Path(root).expanduser() / "herdr-remote-download"
    return Path.home() / ".config" / "herdr-remote-download"


def default_token_path() -> Path:
    return default_config_dir() / "token"


def default_data_dir() -> Path:
    root = os.environ.get("XDG_DATA_HOME")
    if root:
        return Path(root).expanduser() / "herdr-remote-download"
    return Path.home() / ".local" / "share" / "herdr-remote-download"


def default_remote_socket_path() -> Path:
    return Path(f"/tmp/herdr-remote-download-{getpass.getuser()}.sock")


def _validate_token(token: str) -> str:
    token = token.strip()
    if len(token) != TOKEN_HEX_LENGTH or not re.fullmatch(r"[0-9a-fA-F]+", token):
        raise DownloadError("token must contain exactly 64 hexadecimal characters")
    return token.lower()


def read_token(path: Path) -> str:
    try:
        return _validate_token(path.read_text(encoding="ascii"))
    except FileNotFoundError as error:
        raise DownloadError(f"token file not found: {path}") from error
    except OSError as error:
        raise DownloadError(f"cannot read token file {path}: {error}") from error


def ensure_token(path: Path) -> str:
    if path.exists():
        return read_token(path)

    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    token = secrets.token_hex(32)
    try:
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        return read_token(path)
    try:
        with os.fdopen(descriptor, "w", encoding="ascii") as token_file:
            token_file.write(f"{token}\n")
    except Exception:
        try:
            path.unlink()
        except OSError:
            pass
        raise
    return token


def sender_token_path() -> Path:
    explicit = os.environ.get("HERDR_DOWNLOAD_TOKEN_FILE")
    if explicit:
        return Path(explicit).expanduser()

    plugin_config = os.environ.get("HERDR_PLUGIN_CONFIG_DIR")
    if plugin_config:
        plugin_token = Path(plugin_config) / "token"
        if plugin_token.exists():
            return plugin_token

    return default_token_path()


def encode_header_value(value: str) -> str:
    return base64.urlsafe_b64encode(value.encode("utf-8")).decode("ascii").rstrip("=")


def decode_header_value(value: str) -> str:
    if not value or len(value) > 1024:
        raise DownloadError("invalid encoded filename")
    try:
        padding = "=" * (-len(value) % 4)
        decoded = base64.urlsafe_b64decode(f"{value}{padding}")
        return decoded.decode("utf-8")
    except (binascii.Error, UnicodeDecodeError) as error:
        raise DownloadError("invalid encoded filename") from error


def _clean_filename(value: str) -> str:
    name = value.replace("\\", "/").rsplit("/", 1)[-1].strip()
    if (
        not name
        or name in {".", ".."}
        or "\x00" in name
        or any(ord(character) < 32 for character in name)
        or len(name.encode("utf-8")) > 240
    ):
        raise DownloadError("unsafe or unsupported filename")
    return name


def _strip_path_markup(value: str) -> str:
    value = value.strip()
    markdown = re.fullmatch(r"\[[^\]]*]\((.+)\)", value)
    if markdown:
        value = markdown.group(1).strip()
    if len(value) >= 2 and (
        (value[0], value[-1]) in {("`", "`"), ("'", "'"), ('"', '"'), ("<", ">")}
    ):
        value = value[1:-1].strip()
    return value


def _path_from_file_url(value: str) -> str:
    parsed = urlparse(value)
    if parsed.scheme != "file":
        return value

    allowed_hosts = {
        "",
        "localhost",
        socket.gethostname(),
        socket.getfqdn(),
    }
    if parsed.netloc not in allowed_hosts:
        raise DownloadError(f"file URL belongs to another host: {parsed.netloc}")
    return unquote(parsed.path)


def resolve_path_from_context(context: dict[str, Any]) -> Path:
    raw_value = context.get("clicked_url") or context.get("selected_text")
    if not isinstance(raw_value, str) or not raw_value.strip():
        raise DownloadError("select one remote file path or click a file:// link first")

    nonempty_lines = [line.strip() for line in raw_value.splitlines() if line.strip()]
    if len(nonempty_lines) != 1:
        raise DownloadError("select exactly one file path")

    candidate = _path_from_file_url(_strip_path_markup(nonempty_lines[0]))
    if "\x00" in candidate:
        raise DownloadError("file path contains a null byte")

    cwd_value = context.get("focused_pane_cwd") or context.get("workspace_cwd")
    base = Path(cwd_value).expanduser() if isinstance(cwd_value, str) else Path.cwd()

    candidate_values = [candidate]
    line_suffix = re.fullmatch(r"(.+?):[0-9]+(?::[0-9]+)?", candidate)
    if line_suffix:
        candidate_values.append(line_suffix.group(1))

    checked: list[Path] = []
    for candidate_value in candidate_values:
        path = Path(candidate_value).expanduser()
        if not path.is_absolute():
            path = base / path
        path = path.resolve(strict=False)
        checked.append(path)
        if path.is_file():
            return path

    shown = checked[-1] if checked else candidate
    raise DownloadError(f"file does not exist or is not a regular file: {shown}")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


class UnixHTTPConnection(http.client.HTTPConnection):
    def __init__(self, socket_path: Path, timeout: int):
        super().__init__("localhost", timeout=timeout)
        self.socket_path = socket_path

    def connect(self) -> None:
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(str(self.socket_path))


def _receiver_connection(
    host: str,
    port: int,
    timeout: int,
    socket_path: Optional[Path],
) -> http.client.HTTPConnection:
    if socket_path is not None:
        return UnixHTTPConnection(socket_path, timeout)
    return http.client.HTTPConnection(host, port, timeout=timeout)


def _receiver_endpoint(host: str, port: int, socket_path: Optional[Path]) -> str:
    if socket_path is not None:
        return str(socket_path)
    return f"{host}:{port}"


def check_receiver(
    host: str,
    port: int,
    timeout: int,
    socket_path: Optional[Path] = None,
) -> None:
    preflight_timeout = min(timeout, PREFLIGHT_TIMEOUT_SECONDS)
    if preflight_timeout <= 0:
        raise DownloadError("transfer timeout must be greater than zero")

    endpoint = _receiver_endpoint(host, port, socket_path)
    connection = _receiver_connection(host, port, preflight_timeout, socket_path)
    try:
        connection.request("GET", "/health")
        response = connection.getresponse()
        payload_bytes = response.read(16 * 1024)
    except (OSError, http.client.HTTPException) as error:
        raise DownloadError(
            f"local receiver is unavailable through {endpoint}; "
            "reconnect the Herdr remote session and verify its SSH RemoteForward"
        ) from error
    finally:
        connection.close()

    try:
        payload = json.loads(payload_bytes.decode("utf-8")) if payload_bytes else {}
    except (UnicodeDecodeError, json.JSONDecodeError):
        payload = {}
    if (
        response.status != 200
        or not isinstance(payload, dict)
        or payload.get("service") != "herdr-remote-download"
        or payload.get("status") != "ok"
    ):
        raise DownloadError(
            f"unexpected service through {endpoint}; "
            "verify the Herdr remote-download SSH RemoteForward"
        )


def upload_file(
    path: Path,
    *,
    host: str,
    port: int,
    token: str,
    timeout: int,
    max_bytes: int = DEFAULT_MAX_BYTES,
    socket_path: Optional[Path] = None,
) -> dict[str, Any]:
    token = _validate_token(token)
    try:
        size = path.stat().st_size
    except OSError as error:
        raise DownloadError(f"cannot inspect file {path}: {error}") from error
    if not path.is_file():
        raise DownloadError(f"not a regular file: {path}")
    if size > max_bytes:
        raise DownloadError(f"file is larger than the {max_bytes} byte transfer limit")

    check_receiver(host, port, timeout, socket_path)
    checksum = sha256_file(path)
    endpoint = _receiver_endpoint(host, port, socket_path)
    connection = _receiver_connection(host, port, timeout, socket_path)
    try:
        connection.putrequest("POST", TRANSFER_PATH)
        connection.putheader("Authorization", f"Bearer {token}")
        connection.putheader("Content-Length", str(size))
        connection.putheader("Content-Type", "application/octet-stream")
        connection.putheader("X-Herdr-Filename", encode_header_value(path.name))
        connection.putheader("X-Herdr-SHA256", checksum)
        connection.endheaders()
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                connection.send(chunk)
        response = connection.getresponse()
        payload_bytes = response.read(64 * 1024)
    except (OSError, http.client.HTTPException) as error:
        raise DownloadError(
            f"cannot reach the local receiver through {endpoint}: {error}"
        ) from error
    finally:
        connection.close()

    try:
        payload = json.loads(payload_bytes.decode("utf-8")) if payload_bytes else {}
    except (UnicodeDecodeError, json.JSONDecodeError):
        payload = {}
    if response.status != 201:
        detail = payload.get("error") if isinstance(payload, dict) else None
        suffix = f": {detail}" if detail else ""
        raise DownloadError(f"receiver returned HTTP {response.status}{suffix}")
    if not isinstance(payload, dict):
        raise DownloadError("receiver returned an invalid response")
    return payload


def _destination_candidates(directory: Path, filename: str):
    yield directory / filename
    source_name = Path(filename)
    for index in range(1, 10_000):
        yield directory / f"{source_name.stem} ({index}){source_name.suffix}"


def _commit_without_overwrite(
    temporary_path: Path, directory: Path, filename: str
) -> Path:
    for candidate in _destination_candidates(directory, filename):
        try:
            os.link(temporary_path, candidate)
        except FileExistsError:
            continue
        temporary_path.unlink()
        return candidate
    raise DownloadError("could not allocate a unique destination filename")


class DownloadHTTPServer(HTTPServer):
    allow_reuse_address = True

    def __init__(
        self,
        server_address: tuple[str, int],
        *,
        destination: Path,
        token: str,
        max_bytes: int,
        verbose: bool,
    ):
        self.destination = destination
        self.token = _validate_token(token)
        self.max_bytes = max_bytes
        self.verbose = verbose
        super().__init__(server_address, DownloadRequestHandler)


class DownloadRequestHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: DownloadHTTPServer

    def log_message(self, format_string: str, *arguments: Any) -> None:
        if self.server.verbose:
            super().log_message(format_string, *arguments)

    def _respond(self, status: int, payload: dict[str, Any]) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def do_GET(self) -> None:
        if self.path != "/health":
            self._respond(404, {"error": "not found"})
            return
        self._respond(
            200,
            {
                "service": "herdr-remote-download",
                "status": "ok",
                "destination": str(self.server.destination),
            },
        )

    def do_POST(self) -> None:
        if self.path != TRANSFER_PATH:
            self._respond(404, {"error": "not found"})
            return

        expected = f"Bearer {self.server.token}"
        provided = self.headers.get("Authorization", "")
        if not secrets.compare_digest(provided, expected):
            self._respond(401, {"error": "unauthorized"})
            return

        try:
            length = int(self.headers.get("Content-Length", ""))
        except ValueError:
            self._respond(411, {"error": "valid Content-Length required"})
            return
        if length < 0 or length > self.server.max_bytes:
            self._respond(413, {"error": "file exceeds receiver limit"})
            return

        try:
            filename = _clean_filename(
                decode_header_value(self.headers.get("X-Herdr-Filename", ""))
            )
        except DownloadError as error:
            self._respond(400, {"error": str(error)})
            return

        expected_checksum = self.headers.get("X-Herdr-SHA256", "").lower()
        if not re.fullmatch(r"[0-9a-f]{64}", expected_checksum):
            self._respond(400, {"error": "valid SHA-256 header required"})
            return

        self.server.destination.mkdir(mode=0o700, parents=True, exist_ok=True)
        descriptor, temporary_name = tempfile.mkstemp(
            prefix=".herdr-download-",
            suffix=".part",
            dir=self.server.destination,
        )
        temporary_path = Path(temporary_name)
        digest = hashlib.sha256()
        remaining = length
        try:
            with os.fdopen(descriptor, "wb") as destination:
                while remaining:
                    chunk = self.rfile.read(min(1024 * 1024, remaining))
                    if not chunk:
                        raise DownloadError("request body ended before Content-Length")
                    destination.write(chunk)
                    digest.update(chunk)
                    remaining -= len(chunk)
                destination.flush()
                os.fsync(destination.fileno())

            actual_checksum = digest.hexdigest()
            if actual_checksum != expected_checksum:
                raise DownloadError("SHA-256 mismatch")

            final_path = _commit_without_overwrite(
                temporary_path, self.server.destination, filename
            )
        except DownloadError as error:
            try:
                temporary_path.unlink()
            except OSError:
                pass
            status = 422 if "SHA-256" in str(error) else 400
            self._respond(status, {"error": str(error)})
            return
        except OSError as error:
            try:
                temporary_path.unlink()
            except OSError:
                pass
            self._respond(500, {"error": f"cannot save file: {error}"})
            return

        self._respond(
            201,
            {
                "path": str(final_path),
                "bytes": length,
                "sha256": actual_checksum,
            },
        )


def _notify_herdr(title: str, body: str, sound: str) -> None:
    herdr = os.environ.get("HERDR_BIN_PATH")
    if not herdr:
        return
    try:
        subprocess.run(
            [herdr, "notification", "show", title, "--body", body, "--sound", sound],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    except OSError:
        pass


def _context_from_environment() -> dict[str, Any]:
    raw_context = os.environ.get("HERDR_PLUGIN_CONTEXT_JSON", "{}")
    try:
        context = json.loads(raw_context)
    except json.JSONDecodeError as error:
        raise DownloadError("HERDR_PLUGIN_CONTEXT_JSON is invalid") from error
    if not isinstance(context, dict):
        raise DownloadError("HERDR_PLUGIN_CONTEXT_JSON must be an object")
    clicked_url = os.environ.get("HERDR_PLUGIN_CLICKED_URL")
    if clicked_url:
        context["clicked_url"] = clicked_url
    return context


def command_send_context(arguments: argparse.Namespace) -> int:
    try:
        context = _context_from_environment()
        path = resolve_path_from_context(context)
        token_path = sender_token_path()
        token = read_token(token_path)
        result = upload_file(
            path,
            host="127.0.0.1",
            port=arguments.port,
            token=token,
            timeout=arguments.timeout,
            max_bytes=arguments.max_mb * 1024 * 1024,
            socket_path=arguments.socket or default_remote_socket_path(),
        )
    except DownloadError as error:
        _notify_herdr("Remote download failed", str(error), "request")
        print(f"herdr-remote-download: {error}", file=sys.stderr)
        return 1

    _notify_herdr(
        f"Downloaded {path.name}",
        str(result.get("path", "Saved on the connected machine")),
        "done",
    )
    print(json.dumps(result, ensure_ascii=False))
    return 0


def command_send(arguments: argparse.Namespace) -> int:
    path = Path(arguments.path).expanduser().resolve()
    try:
        result = upload_file(
            path,
            host=arguments.host,
            port=arguments.port,
            token=read_token(arguments.token_file),
            timeout=arguments.timeout,
            max_bytes=arguments.max_mb * 1024 * 1024,
            socket_path=arguments.socket,
        )
    except DownloadError as error:
        print(f"herdr-remote-download: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, ensure_ascii=False))
    return 0


def command_init_token(arguments: argparse.Namespace) -> int:
    try:
        ensure_token(arguments.token_file)
    except (DownloadError, OSError) as error:
        print(f"herdr-remote-download: {error}", file=sys.stderr)
        return 1
    print(arguments.token_file)
    return 0


def command_serve(arguments: argparse.Namespace) -> int:
    try:
        token = ensure_token(arguments.token_file)
        server = DownloadHTTPServer(
            (arguments.host, arguments.port),
            destination=arguments.download_dir,
            token=token,
            max_bytes=arguments.max_mb * 1024 * 1024,
            verbose=arguments.verbose,
        )
    except (DownloadError, OSError) as error:
        print(f"herdr-remote-download: {error}", file=sys.stderr)
        return 1

    print(
        f"herdr-remote-download listening on "
        f"http://{arguments.host}:{server.server_address[1]} "
        f"and saving to {arguments.download_dir}",
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


def _launchd_domain() -> str:
    return f"gui/{os.getuid()}"


def _launchd_plist(
    *,
    python_path: Path,
    script_path: Path,
    token_path: Path,
    download_dir: Path,
    port: int,
    log_path: Path,
) -> dict[str, Any]:
    return {
        "Label": LAUNCHD_LABEL,
        "ProgramArguments": [
            str(python_path),
            str(script_path),
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            str(port),
            "--download-dir",
            str(download_dir),
            "--token-file",
            str(token_path),
        ],
        "RunAtLoad": True,
        "KeepAlive": True,
        "ProcessType": "Background",
        "StandardOutPath": str(log_path),
        "StandardErrorPath": str(log_path),
        "EnvironmentVariables": {"PYTHONUNBUFFERED": "1"},
    }


def command_install_service(arguments: argparse.Namespace) -> int:
    if sys.platform != "darwin":
        print("herdr-remote-download: launchd installation requires macOS", file=sys.stderr)
        return 1
    if not arguments.python.exists():
        print(
            f"herdr-remote-download: Python executable not found: {arguments.python}",
            file=sys.stderr,
        )
        return 1

    try:
        ensure_token(arguments.token_file)
        install_dir = default_data_dir()
        install_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
        installed_script = install_dir / Path(__file__).name
        shutil.copy2(Path(__file__).resolve(), installed_script)
        installed_script.chmod(0o755)

        arguments.download_dir.mkdir(parents=True, exist_ok=True)
        log_path = Path.home() / "Library" / "Logs" / "herdr-remote-download.log"
        log_path.parent.mkdir(parents=True, exist_ok=True)
        plist_path = (
            Path.home() / "Library" / "LaunchAgents" / f"{LAUNCHD_LABEL}.plist"
        )
        plist_path.parent.mkdir(parents=True, exist_ok=True)
        plist = _launchd_plist(
            python_path=arguments.python,
            script_path=installed_script,
            token_path=arguments.token_file,
            download_dir=arguments.download_dir,
            port=arguments.port,
            log_path=log_path,
        )
        with tempfile.NamedTemporaryFile(
            mode="wb",
            prefix=f".{LAUNCHD_LABEL}.",
            suffix=".plist",
            dir=plist_path.parent,
            delete=False,
        ) as plist_file:
            temporary_plist = Path(plist_file.name)
            plistlib.dump(plist, plist_file, sort_keys=False)
        os.replace(temporary_plist, plist_path)
    except OSError as error:
        print(f"herdr-remote-download: service installation failed: {error}", file=sys.stderr)
        return 1

    domain = _launchd_domain()
    subprocess.run(
        ["/bin/launchctl", "bootout", f"{domain}/{LAUNCHD_LABEL}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    bootstrap = subprocess.run(
        ["/bin/launchctl", "bootstrap", domain, str(plist_path)],
        capture_output=True,
        text=True,
        check=False,
    )
    if bootstrap.returncode != 0:
        detail = bootstrap.stderr.strip() or bootstrap.stdout.strip()
        print(
            f"herdr-remote-download: launchctl bootstrap failed: {detail}",
            file=sys.stderr,
        )
        return 1
    subprocess.run(
        ["/bin/launchctl", "kickstart", "-k", f"{domain}/{LAUNCHD_LABEL}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    print(plist_path)
    return 0


def command_service_status(arguments: argparse.Namespace) -> int:
    del arguments
    if sys.platform != "darwin":
        print("herdr-remote-download: launchd status requires macOS", file=sys.stderr)
        return 1
    result = subprocess.run(
        ["/bin/launchctl", "print", f"{_launchd_domain()}/{LAUNCHD_LABEL}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    if result.returncode != 0:
        print("not running")
        return 1
    print("running")
    return 0


def add_keybinding_config(content: str, key: str) -> tuple[str, bool]:
    if PLUGIN_ACTION in content:
        legacy_description = 'description = "download selected remote file"'
        if legacy_description in content:
            return (
                content.replace(
                    legacy_description,
                    f'description = "{PICK_DESCRIPTION}"',
                    1,
                ),
                True,
            )
        return content, False
    if not re.fullmatch(r"[A-Za-z0-9+_-]+", key):
        raise DownloadError("keybinding contains unsupported characters")
    key_match = re.search(rf'(?m)^key\s*=\s*"{re.escape(key)}"\s*$', content)
    legacy_command = f'command = "{DOWNLOAD_ACTION}"'
    if key_match and legacy_command in content:
        updated = content.replace(legacy_command, f'command = "{PICK_ACTION}"', 1)
        updated = updated.replace(
            'description = "download selected remote file"',
            f'description = "{PICK_DESCRIPTION}"',
            1,
        )
        return updated, True
    if key_match:
        raise DownloadError(f"keybinding is already in use: {key}")

    block = (
        "\n[[keys.command]]\n"
        f'key = "{key}"\n'
        'type = "plugin_action"\n'
        f'command = "{PLUGIN_ACTION}"\n'
        f'description = "{PICK_DESCRIPTION}"\n'
    )
    insertion = re.search(r"(?m)^\[worktrees\]\s*$", content)
    if insertion:
        content = f"{content[:insertion.start()].rstrip()}\n{block}\n{content[insertion.start():]}"
    else:
        content = f"{content.rstrip()}\n{block}"
    return content, True


def command_configure_keybinding(arguments: argparse.Namespace) -> int:
    try:
        content = arguments.config.read_text(encoding="utf-8")
        updated, changed = add_keybinding_config(content, arguments.key)
        if not changed:
            print("already configured")
            return 0

        backup = arguments.config.with_name(
            f"{arguments.config.name}.before-herdr-remote-download.bak"
        )
        if not backup.exists():
            shutil.copy2(arguments.config, backup)

        original_mode = arguments.config.stat().st_mode & 0o777
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            prefix=f".{arguments.config.name}.",
            dir=arguments.config.parent,
            delete=False,
        ) as config_file:
            temporary_config = Path(config_file.name)
            config_file.write(updated)
        temporary_config.chmod(original_mode)
        os.replace(temporary_config, arguments.config)
    except (DownloadError, OSError) as error:
        print(f"herdr-remote-download: {error}", file=sys.stderr)
        return 1

    print(f"configured {arguments.key}")
    return 0


def _add_transfer_options(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--port",
        type=int,
        default=int(os.environ.get("HERDR_DOWNLOAD_PORT", DEFAULT_PORT)),
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=DEFAULT_TIMEOUT_SECONDS,
    )
    parser.add_argument(
        "--socket",
        type=Path,
        default=(
            Path(os.environ["HERDR_DOWNLOAD_SOCKET"])
            if os.environ.get("HERDR_DOWNLOAD_SOCKET")
            else None
        ),
        help="remote Unix socket forwarded to the connected machine",
    )
    parser.add_argument(
        "--max-mb",
        type=int,
        default=DEFAULT_MAX_BYTES // (1024 * 1024),
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Download files from a remote Herdr pane to the connected machine."
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    send_context = subparsers.add_parser(
        "send-context",
        help="send the selected path or clicked file URL from Herdr plugin context",
    )
    _add_transfer_options(send_context)
    send_context.set_defaults(handler=command_send_context)

    send = subparsers.add_parser("send", help="send one explicit file")
    send.add_argument("path")
    send.add_argument("--host", default="127.0.0.1")
    send.add_argument("--token-file", type=Path, default=default_token_path())
    _add_transfer_options(send)
    send.set_defaults(handler=command_send)

    init_token = subparsers.add_parser(
        "init-token", help="create the local authentication token if needed"
    )
    init_token.add_argument("--token-file", type=Path, default=default_token_path())
    init_token.set_defaults(handler=command_init_token)

    serve = subparsers.add_parser("serve", help="run the receiver on the connected machine")
    serve.add_argument("--host", default="127.0.0.1")
    serve.add_argument("--port", type=int, default=DEFAULT_PORT)
    serve.add_argument("--download-dir", type=Path, default=Path.home() / "Downloads")
    serve.add_argument("--token-file", type=Path, default=default_token_path())
    serve.add_argument(
        "--max-mb",
        type=int,
        default=DEFAULT_MAX_BYTES // (1024 * 1024),
    )
    serve.add_argument("--verbose", action="store_true")
    serve.set_defaults(handler=command_serve)

    install_service = subparsers.add_parser(
        "install-service",
        help="install and start the macOS launchd receiver",
    )
    install_service.add_argument("--port", type=int, default=DEFAULT_PORT)
    install_service.add_argument(
        "--download-dir", type=Path, default=Path.home() / "Downloads"
    )
    install_service.add_argument(
        "--token-file", type=Path, default=default_token_path()
    )
    install_service.add_argument(
        "--python", type=Path, default=Path("/usr/bin/python3")
    )
    install_service.set_defaults(handler=command_install_service)

    service_status = subparsers.add_parser(
        "service-status",
        help="report whether the macOS launchd receiver is running",
    )
    service_status.set_defaults(handler=command_service_status)

    configure_keybinding = subparsers.add_parser(
        "configure-keybinding",
        help="add the plugin action to a Herdr server config",
    )
    configure_keybinding.add_argument(
        "--config",
        type=Path,
        default=Path.home() / ".config" / "herdr" / "config.toml",
    )
    configure_keybinding.add_argument("--key", default="prefix+d")
    configure_keybinding.set_defaults(handler=command_configure_keybinding)

    return parser


def main() -> int:
    arguments = build_parser().parse_args()
    if getattr(arguments, "port", DEFAULT_PORT) not in range(1, 65_536):
        print("herdr-remote-download: port must be between 1 and 65535", file=sys.stderr)
        return 2
    if getattr(arguments, "max_mb", 1) <= 0:
        print("herdr-remote-download: --max-mb must be positive", file=sys.stderr)
        return 2
    return arguments.handler(arguments)


if __name__ == "__main__":
    raise SystemExit(main())
