# Herdr Remote File Transfer

English | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

[Changes and upgrade notes](CHANGELOG.md).

A Herdr plugin that transfers files in both directions between a
`herdr --remote` host and the connected Mac.

Compare highlighted conversation context with a full-path file list, select
with arrow keys or hint characters, or transfer files from
`file://` links displayed by Codex, Claude, and other tools. It supports regular
files of any extension, including PDFs, PPTX files, images, and archives;
directories are archived and extracted automatically. In
the other direction, choose one or more Mac files and save them to the focused
remote pane's current directory.

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

### Connecting with `herdr machine add`

`./setup.sh mercury` installs a LaunchAgent for the transfer tunnel. It starts
at Mac login and reconnects after SSH disconnects. Only stale sockets are
removed: ones nothing listens on, or ones left listening by a connection that
dropped with the network, which do not answer a health check within 10 seconds.
SSH authentication must work non-interactively (load your key into the SSH
agent if needed).

```sh
herdr machine add mercury --label "mercury"
```

Skip registration if already registered. Keep the machine target as `mercury`.
The updated `hr mercury` can coexist with saved-machine connections. To add only
automatic recovery to an existing setup, stop your old manual tunnel and run:

```sh
python3 herdr_transfer_tunnel.py install mercury
```

Old `hr` functions delete the socket: rerun `setup.sh` and `source ~/.zshrc`
before using them alongside the LaunchAgent.
Logs: `~/Library/Logs/com.kosukeyano.herdr-transfer-tunnel.mercury.log`.

Check connectivity with:

```sh
ssh mercury 'curl -fsS --max-time 5 --unix-socket /tmp/herdr-remote-download-$(id -un).sock http://localhost/health'
```

Dead SSH connections are normally detected in about 45 seconds; launchd retries
at intervals of at least 30 seconds. If the socket on the remote host is removed
while the tunnel is running, the tunnel reconnects within about 10 seconds.

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
description = "upload files from the connected Mac"
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

### First download after setup

1. Focus the remote pane containing the file reference. With Herdr's default
   prefix, press **Ctrl+B**, release it, then press **d**. If you changed the
   prefix, use your configured prefix followed by `d`.
2. Match a highlighted `[hint]` in the left snapshot to the same hint and full
   path on the right.
3. Press Right to activate the file list, use Up/Down to choose a file, and
   press Enter. You can also type its hint directly.
4. By default, find the saved file in the Mac's `~/Downloads` folder; Finder
   also reveals it. A `:line` suffix identifies a reference; the whole file
   is downloaded.

### Select a visible file path

Press `prefix+d` to open one download overlay with two views. The left view
(about 70% of the width) is a read-only conversation snapshot. Existing paths
are highlighted and labelled `[a]`, `[s]`, and so on without replacing path
characters. The narrower right view lists complete resolved paths with the
same hints. `>` marks the keyboard-focused file; `[x]` marks a file selected
for a batch. A hint label is separate from the path, not part of its filename.
Small terminals stack the views. If the Refresh button is hidden in a narrow
window, Ctrl+R still works.

The snapshot preserves line breaks captured from the source terminal. A line
that already filled the source pane can wrap again in the narrower snapshot
view, so its displayed row count may increase. Scroll to read the remaining
rows; captured lines are not joined because that could change code or prose.

- Left/Right or a mouse click chooses the active view.
- In the file list, Up/Down selects a file and Enter downloads it. PageUp,
  PageDown, Home and End move the selection and keep it visible.
- Shift+Up/Down scrolls the focused path itself when it is taller than the
  file viewport; its hint stays pinned in the title.
- In the conversation snapshot, those navigation keys scroll the text. In
  single mode, Enter shows a reminder to press Right and choose a file before downloading.
- Typing a file's hint still downloads it directly.
- Tab starts multi-select. Type hints or press Space on the focused file to
  toggle selections; Tab or Enter downloads the selected files in order.

Esc closes the download overlay. Both views belong to the picker, so clicking
and scrolling the snapshot does not direct keys to the original agent. The
original pane receives focus again after the overlay closes, so subsequent
key presses act on that original session. During a transfer, Esc or Ctrl+C
cancels the transfer. Opening the picker switches the connected Mac to an ASCII-capable
input source (such as ABC), so hints can be typed while another input method is active.

The snapshot and candidate list are captured once when the picker opens.
Scrolling does not add/remove candidates or change their hints: paths outside
the scrolled viewport remain in the list. New output from the underlying agent
is included only when you click **Refresh** (or press Ctrl+R), or close and
reopen the picker. Refresh reads the source pane's current visible page and
working directory, then rebuilds both views. Candidates absent from that page
are removed; this is not a scan of the entire conversation history. Hints may
change and pending hint input and multi-selections are cleared. A failed refresh
keeps the old snapshot, working directory and selection. Scrolling alone never
changes the candidates. The snapshot shows conversation text, not
file contents.

During a transfer the picker shows the file number, percentage and sent bytes.

The picker recognizes absolute paths, paths relative to the pane's current
working directory, and paths ending in `:line` or `:line:column`. It works with
any existing file shown on screen, not only files printed by Codex or Claude.

You can also invoke the action directly:

```sh
herdr plugin action invoke kosukeyano.remote-download.pick
```

### After a file arrives on the Mac

By default the Mac selects each downloaded file in Finder. To open files with
their default app instead, or to do nothing, reinstall the transfer service
with `--after-save`:

```sh
./target/release/herdr-remote-download install-service --after-save open   # or reveal, none
```

`setup.sh` also reads `HERDR_DOWNLOAD_AFTER_SAVE`. Folders and files that can
run code (such as `.command`, `.sh`, `.app`, or `.pkg`) are only revealed, even
with `open`.

### Download from a `file://` link

Control-click a `file://` link displayed by Codex, Claude, or another tool in
Herdr to transfer its target. On macOS, Control is the modifier Herdr captures
for link actions.

### Download from selected text

Select a file path and invoke the
`kosukeyano.remote-download.download` action directly.

### Upload from the Mac

Focus the destination pane and press `prefix+u`. Choose one or more regular
files in the Mac dialog using Command or Shift. The plugin saves them in that
pane's current directory. The overlay shows the current file number, filename,
percentage, and received bytes during transfer, then the saved paths. Press
Enter to close it.

```sh
herdr plugin action invoke kosukeyano.remote-download.upload
```

## Limitations

- Directories are archived as `.tar.gz` automatically and extracted into a new folder in the download directory. Symlinks and special files inside the archive are skipped.
- Files larger than 512 MiB each are rejected by default. Set `HERDR_DOWNLOAD_MAX_MB` on both sides (for example `2048`, or `0` for unlimited), or pass `--max-mb`.
- Automatic transfer-service startup uses launchd on macOS and a systemd user service on Linux.
- The SSH `RemoteForward` and authentication token must be configured for each remote host.

## Troubleshooting

| Symptom | What to do |
| --- | --- |
| No candidate for a path | Ask the agent to print the complete absolute path. Relative paths are resolved from the source pane's directory; omitted directory prefixes are not searched for. |
| New output is missing | Click Refresh or press Ctrl+R. Scrolling the snapshot does not reread the agent's terminal. |
| `cannot read token file` | Finish the Mac receiver and token-copy setup. Installing the remote plugin alone does not pair the Mac. |
| Enter reports a window-size problem | Enlarge the terminal so the selected file can be displayed. |

If the connection reports `remote port forwarding failed for listen path`, run
the `rm -f` command shown above and reconnect.

If the SSH tunnel is lost during a transfer, the picker displays the failure
after you select the file: within about a second when the tunnel is down, or
within about 20 seconds when it does not answer. On a lossy network the first
answer through an idle tunnel can be slow, so the check is retried once
automatically. Press Esc or Enter to close it.
With the LaunchAgent tunnel, retry after about a minute; each connection attempt
is timestamped in its log. With the manual setup, reconnect Herdr.

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
