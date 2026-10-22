# Changelog

## Unreleased

### Changed

- Compare highlighted conversation context with a complete file-path list in one
  download overlay. Hint labels preserve the original path characters.
- Navigate files with arrow keys, download with Enter, and explicitly refresh
  the snapshot and source working directory with Refresh or Ctrl+R.

### Upgrading an existing installation

A picker that was already open before a plugin update keeps running its old
version. Click that download window, press Esc once to close it, then reopen
the picker from the source pane. The new picker uses the existing receiver and
pairing configuration.
