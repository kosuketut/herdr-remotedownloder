# Changelog

All notable changes to this project are recorded here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Dates are release tag
dates.

## [Unreleased]

### Changed

- The English, Japanese, and Simplified Chinese READMEs present download and
  upload side by side, with a usage section for each direction.

## [0.7.0] - 2026-10-23

### Added

- Opening the picker switches the connected Mac to an ASCII-capable input source
  (such as ABC), so hint keys work while a Japanese or other input method is
  active.

### Changed

- Compare highlighted conversation context with a complete file-path list in one
  download overlay. Hint labels preserve the original path characters.
- Navigate files with arrow keys, download with Enter, and explicitly refresh
  the snapshot and source working directory with Refresh or Ctrl+R.

### Fixed

- Wait up to 10 seconds for the receiver health check and retry it once, so a
  slow first request through an idle tunnel no longer fails the transfer. A
  timed-out check is reported as a slow network instead of a disconnected
  tunnel.
- Replace a remote tunnel socket that still listens but no longer answers, so
  the tunnel reconnects after the network drops an SSH session.
- Reload the Herdr config during setup only when the Herdr server is running.

### Upgrading an existing installation

On the connected Mac, update the checkout and run setup again so the receiver,
tunnel, and remote plugin are all updated:

    git pull
    ./setup.sh your-server

A picker that was already open before a plugin update keeps running its old
version. Click that download window, press Esc once to close it, then reopen
the picker from the source pane. The new picker uses the existing receiver and
pairing configuration.

## [0.6.0] - 2026-10-09

### Added

- Select several files in the download picker: press Tab, type each hint
  (typing it again deselects), then press Tab or Enter. Files are sent one after
  another, and one notification summarizes the result.
- Show progress while downloading: archiving, checksum, then a progress bar with
  percentage and MiB sent.
- Choose what the Mac does after a download arrives with
  `install-service --after-save none|reveal|open` (or
  `HERDR_DOWNLOAD_AFTER_SAVE`). The default `reveal` selects the file in Finder.
  `open` opens it with its default app; folders and files that can run code
  (`.command`, `.sh`, `.app`, `.pkg`, and similar) are only revealed.
- Offer bare directory names in the picker.
- Cancel a download with Esc or Ctrl+C.

### Changed

- Keep the Mac transfer tunnel running as a per-host LaunchAgent, independently
  of Herdr. It reconnects after SSH disconnects and within about 10 seconds if
  the remote socket is removed. The tunnel log is timestamped.

### Fixed

- Wrap long transfer errors instead of cutting them off.
- Preserve user tool paths in remote setup.
- Pass `--yes` after the repository argument to `herdr plugin install`.

### Install or upgrade

On the connected Mac, update the checkout and run:

    git pull
    ./setup.sh your-server
    source ~/.zshrc
    hr your-server

Replace `your-server` with an existing SSH config host. The setup command can be
run again without duplicating its managed settings.

## [0.5.0] - 2026-08-22

No GitHub Release was published for this tag; its changes were announced with
0.6.0.

### Added

- Transfer directories: they are archived as `.tar.gz` and extracted on the
  connected Mac.
- Configure the size limit with `HERDR_DOWNLOAD_MAX_MB` or `--max-mb`; `0`
  disables it.
- Support systemd user services on Linux in `install-service` and
  `service-status`.
- Upload several Mac files at once.

## [0.4.0] - 2026-08-01

### Added

- Bidirectional file transfer. In addition to remote-to-Mac downloads, prefix+u
  opens the connected Mac file picker and uploads the selected file to the
  focused remote pane directory.
- `setup.sh` builds and starts the Mac service, installs the remote plugin,
  copies the token, configures both key bindings, creates the SSH forwarding
  host, and installs the `hr` command in one run.

### Changed

- Keep token authentication, SHA-256 verification, the 512 MiB default limit,
  and no-overwrite behavior in both directions.
- Setup is repeatable, keeps configuration backups, supports multiple SSH hosts,
  and retains effective SSH options such as identity files and ProxyJump.
- Update the English, Japanese, and Simplified Chinese documentation and add
  isolated setup coverage.

### Install or upgrade

On the connected Mac, update the checkout and run:

    git pull
    ./setup.sh your-server
    source ~/.zshrc
    hr your-server

Replace `your-server` with an existing SSH config host. The setup command can be
run again without duplicating its managed settings.

## [0.3.0] - 2026-07-30

### Changed

- Replace the Python sender, receiver, launchd management, and keybinding
  configuration with a unified Rust CLI. The picker and complete transfer stack
  are now implemented in Rust.
- Preserve the authenticated HTTP-over-SSH Unix-socket protocol, SHA-256
  verification, no-overwrite behavior, and 512 MiB default transfer limit.
- Add Rust coverage for TCP and Unix-socket transfers, token rejection, checksum
  failures, path parsing, launchd configuration, and keybinding migration.
- Update the English, Japanese, and Simplified Chinese documentation for the
  Rust-only setup.

### Upgrade

On the connected Mac, rebuild and reinstall the receiver:

```sh
cargo build --release --locked
./target/release/herdr-remote-download install-service
```

Then update or reinstall the plugin on the remote host. Existing authentication
tokens and SSH `RemoteForward` settings remain compatible.

## [0.2.1] - 2026-07-30

### Changed

- Document a reusable `hr your-server` wrapper that removes the stale per-user
  Unix socket before starting Herdr.
- Explicitly remove an existing `hr` alias before defining the wrapper, so
  `source ~/.zshrc` reliably migrates an already-open shell.
- Add a verification step showing that `hr` resolves to a function.

The transfer protocol is unchanged from 0.2.0.

## [0.2.0] - 2026-07-29

### Changed

- Replace the remote TCP listener with a per-user Unix socket to isolate Herdr
  SSH sessions.
- Show transfer progress and actionable failures in the picker instead of a
  blank overlay.
- Check receiver availability before hashing or uploading and fail within three
  seconds.
- Document a dedicated SSH host and stale-socket cleanup workflow.

### Upgrade

Update your SSH `RemoteForward` as described in the README, then detach and
reconnect Herdr.

## [0.1.0] - 2026-07-29

### Added

- Initial release: a Herdr plugin that downloads files from a `herdr --remote`
  host to the connected Mac's `~/Downloads`.
- Select visible file paths with hint characters, or transfer files from
  `file://` links shown by Codex, Claude, and other tools.
- Transfer only over an SSH `RemoteForward` loopback connection, authenticated
  with a random token and verified with SHA-256. The default limit is 512 MiB,
  and existing files are never overwritten.

[Unreleased]: https://github.com/kosuketut/herdr-remotedownloder/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/kosuketut/herdr-remotedownloder/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/kosuketut/herdr-remotedownloder/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/kosuketut/herdr-remotedownloder/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/kosuketut/herdr-remotedownloder/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/kosuketut/herdr-remotedownloder/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/kosuketut/herdr-remotedownloder/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/kosuketut/herdr-remotedownloder/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/kosuketut/herdr-remotedownloder/releases/tag/v0.1.0
