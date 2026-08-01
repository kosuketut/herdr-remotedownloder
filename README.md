# Herdr Remote File Transfer

English | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

A Herdr plugin that transfers files in both directions between a
`herdr --remote` host and the connected Mac.

Select visible file paths with hint characters, or transfer files from
`file://` links displayed by Codex, Claude, and other tools. It supports regular
files of any extension, including PDFs, PPTX files, images, and archives. In
the other direction, choose a Mac file and save it to the focused remote pane's
current directory.

## How it works

Herdr plugins run on the remote host, so a small transfer service runs on the
connected Mac. Transfers use only an SSH `RemoteForward`; no public port is
opened. The Mac service and remote-to-Mac download path are implemented in
Rust. The Mac-to-remote receiver uses only the Python standard library.

```text
remote Herdr plugin
  -> /tmp/herdr-remote-download-<remote-user>.sock
  -> SSH RemoteForward
  -> 127.0.0.1:18340 on the connected Mac
  <-> ~/Downloads or macOS file picker
  <-> focused remote pane cwd
```

Transfers are authenticated with a random 64-character token and verified with
SHA-256 after receipt. The default size limit is 512 MiB. Existing files are
never overwritten; a duplicate is saved as `name (1).ext`, for example.

## Requirements

- Herdr 0.7.0 or later
- Connected machine: macOS, Git, and Rust/Cargo 1.88 or later
- Remote host: Linux or macOS, Git, Rust/Cargo 1.88 or later, and Python 3.9 or later
- A remote host defined in your SSH config

## Setup

### Automatic setup (recommended)

Make sure the existing SSH target works with `ssh your-server`, then run on
the Mac:

```sh
git clone https://github.com/kosuketut/herdr-remotedownloder.git
cd herdr-remotedownloder
./setup.sh your-server
```

This one command builds and starts the Mac service, installs the remote plugin,
copies the authentication token, configures both key bindings, creates the
forwarding-only SSH host, and installs the `hr` function. Reload the shell and
connect:

```sh
source ~/.zshrc
hr your-server
```

Run `setup.sh` again with another SSH host to add it. Repeating it for the same
host does not duplicate the managed settings. Before the first change, the
script saves `~/.ssh/config` and `~/.zshrc` with a
`.before-herdr-remote-download.bak` suffix.

### Manual setup

Use the following steps when the automatic setup is not available.
The examples below use `your-server` as the existing SSH host name.

#### 1. Transfer service on the Mac

Clone the repository on the Mac and install the launchd service.

```sh
git clone https://github.com/kosuketut/herdr-remotedownloder.git
cd herdr-remotedownloder
cargo build --release --locked
./target/release/herdr-remote-download install-service
./target/release/herdr-remote-download service-status
curl -fsS http://127.0.0.1:18340/health
```

The service binds only to `127.0.0.1` and writes logs to
`~/Library/Logs/herdr-remote-download.log`. Its authentication token is created
at `~/.config/herdr-remote-download/token`.

#### 2. SSH tunnel

Add a dedicated Herdr host to `~/.ssh/config` on the Mac. Use a different name
from the normal SSH host so the forwarding configuration remains isolated.
Place this block before `Host *`.

```sshconfig
Host your-server-herdr
    HostName server.example.com
    User your-name
    IdentityFile ~/.ssh/id_ed25519
    IdentitiesOnly yes
    RemoteForward /tmp/herdr-remote-download-your-name.sock 127.0.0.1:18340
    StreamLocalBindUnlink yes
    ExitOnForwardFailure yes
    ServerAliveInterval 15
    ServerAliveCountMax 3
    ControlMaster no
    ControlPath none
```

Replace `your-name` in the socket path with the user name returned by `id -un`
on the remote host. The Unix socket prevents old and new SSH sessions from
sharing the same TCP forwarding port. `ExitOnForwardFailure yes` stops Herdr at
connection time if the socket cannot be created.

#### 3. Install the plugin on the remote host

Run:

```sh
herdr plugin install kosuketut/herdr-remotedownloder
```

After you confirm the installation, the installer builds both Rust binaries
according to the plugin manifest. The upload client needs no additional build.

#### 4. Copy the authentication token to the remote host

Run this on the Mac:

```sh
ssh your-server \
  'config_dir=$(herdr plugin config-dir kosukeyano.remote-download) &&
   mkdir -p "$config_dir" &&
   umask 077 &&
   cat > "$config_dir/token"' \
  < ~/.config/herdr-remote-download/token
```

Do not add the token to a repository or share it with anyone else.

#### 5. Key binding

Add the following block to `~/.config/herdr/config.toml` on the remote host:

```toml
[[keys.command]]
key = "prefix+d"
type = "plugin_action"
command = "kosukeyano.remote-download.pick"
description = "pick a remote file to download"

[[keys.command]]
key = "prefix+u"
type = "plugin_action"
command = "kosukeyano.remote-download.upload"
description = "upload a file from the connected Mac"
```

Reload the configuration:

```sh
herdr server reload-config
```

To use server-side key bindings in a remote session, remove any stale socket
before connecting. Even if an old SSH process remains, its unlinked socket is
unreachable and the new connection creates the path again.

```sh
ssh your-server 'rm -f /tmp/herdr-remote-download-your-name.sock'
herdr --remote your-server-herdr --remote-keybindings server
```

To keep using `hr your-server` for these two commands, add the following
function to `~/.zshrc` on the Mac. Replace `your-server` and `your-name` with the
same values used above.

```zsh
unalias hr 2>/dev/null
hr() {
  if [[ "$1" == "your-server" ]]; then
    shift
    command ssh your-server \
      'rm -f /tmp/herdr-remote-download-your-name.sock' || return
    command herdr --remote-keybindings server --remote your-server-herdr "$@"
  else
    command herdr --remote-keybindings server --remote "$@"
  fi
}
compdef _herdr hr
```

`unalias` ensures that an old `hr` alias in the current shell is replaced by
the function. Reload the configuration once and verify the result:

```sh
source ~/.zshrc
whence -w hr
# hr: function
```

## Usage

### Select a visible file path

Press `prefix+d` to add hint characters to existing file paths on the visible
screen. Type a hint to transfer that file to the Mac. Press Esc to close the
picker.

The picker recognizes absolute paths, paths relative to the pane's current
working directory, and paths ending in `:line` or `:line:column`. It works with
any existing file shown on screen, not only files printed by Codex or Claude.

You can also invoke the action directly:

```sh
herdr plugin action invoke kosukeyano.remote-download.pick
```

### Download from a `file://` link

Control-click a `file://` link displayed by Codex, Claude, or another tool in
Herdr to transfer its target. On macOS, Control is the modifier Herdr captures
for link actions.

### Download from selected text

Select a file path and invoke the
`kosukeyano.remote-download.download` action directly.

### Upload from the Mac

Focus the destination pane and press `prefix+u`. Choose one regular file in
the Mac dialog. The plugin saves it in that pane's current directory and shows
the saved path in an overlay. Press Enter to close it.

```sh
herdr plugin action invoke kosukeyano.remote-download.upload
```

## Limitations

- Directories cannot be transferred. Archive a directory first.
- Mac-to-remote upload accepts one regular file per invocation.
- Files larger than 512 MiB are rejected by default.
- Automatic transfer-service startup through launchd is supported only on macOS.
- The SSH `RemoteForward` and authentication token must be configured for each remote host.

## Troubleshooting

If the connection reports `remote port forwarding failed for listen path`, run
the `rm -f` command shown above and reconnect.

If the SSH tunnel is lost during a transfer, the picker displays the failure
within three seconds of selecting the file. Press Esc or Enter to close it,
then reconnect Herdr.

## Development

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
python3 -m unittest discover -s tests -v
python3 -m py_compile herdr_remote_upload.py
tests/test_setup.sh
```

The picker UI and hint assignment use
[herdr-tiny-fingers](https://github.com/hotchpotch/herdr-tiny-fingers).
See [`LICENSES`](LICENSES) for third-party license information.

This project is released under the [MIT License](LICENSE).
