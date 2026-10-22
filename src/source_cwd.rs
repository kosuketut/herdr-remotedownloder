//! Read the source pane's current directory for a new snapshot generation.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub(crate) fn read(socket_path: &Path, pane_id: &str) -> Result<PathBuf> {
    let mut stream = UnixStream::connect(socket_path).context("cannot inspect source pane")?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let id = "remote-download:source-cwd";
    let request = json!({"id": id, "method": "pane.get", "params": {"pane_id": pane_id}});
    writeln!(stream, "{request}")?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    let response: Value =
        serde_json::from_str(&response).context("invalid source pane response")?;
    if let Some(error) = response.get("error") {
        bail!("cannot inspect source pane: {error}");
    }
    if response["id"] != id || response["result"]["type"] != "pane_info" {
        bail!("unexpected source pane response");
    }
    directory(&response["result"]["pane"], pane_id)
}

fn directory(pane: &Value, pane_id: &str) -> Result<PathBuf> {
    if pane["pane_id"].as_str() != Some(pane_id) {
        bail!("source pane identity changed");
    }
    let value = pane["foreground_cwd"]
        .as_str()
        .filter(|value| !value.is_empty())
        .or_else(|| pane["cwd"].as_str().filter(|value| !value.is_empty()))
        .context("source pane has no current directory")?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        bail!("source pane directory is not absolute");
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_directory_wins_and_unknown_source_is_rejected() {
        let pane = json!({"pane_id":"source", "cwd":"/old", "foreground_cwd":"/new"});
        assert_eq!(directory(&pane, "source").unwrap(), Path::new("/new"));
        assert!(directory(&pane, "other").is_err());
        assert!(directory(&json!({"pane_id":"source"}), "source").is_err());
        assert!(directory(&json!({"pane_id":"source","cwd":"relative"}), "source").is_err());
        assert_eq!(
            directory(&json!({"pane_id":"source","cwd":"/fallback"}), "source").unwrap(),
            Path::new("/fallback")
        );
    }
}
