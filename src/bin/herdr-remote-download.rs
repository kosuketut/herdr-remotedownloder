use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use herdr_remote_download::transfer::{
    configure_keybinding, context_from_environment, default_download_dir,
    default_herdr_config_path, default_remote_socket_path, default_token_path, ensure_token,
    install_service, notify_herdr, read_token, resolve_path_from_context, run_server,
    sender_token_path, service_status, upload_file, ReceiverEndpoint, ServerConfig,
    DEFAULT_MAX_BYTES, DEFAULT_PORT, DEFAULT_TIMEOUT_SECONDS,
};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(
    name = "herdr-remote-download",
    version,
    about = "Transfer files selected in a remote Herdr session to the connected machine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Transfer the path selected from the current Herdr screen.
    SendContext(TransferOptions),

    /// Transfer an explicit path.
    Send {
        path: PathBuf,

        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        #[arg(long)]
        token_file: Option<PathBuf>,

        #[command(flatten)]
        transfer: TransferOptions,
    },

    /// Create the shared authentication token.
    InitToken {
        #[arg(long)]
        token_file: Option<PathBuf>,
    },

    /// Run the local receiver in the foreground.
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        #[arg(long, env = "HERDR_DOWNLOAD_PORT", default_value_t = DEFAULT_PORT)]
        port: u16,

        #[arg(long)]
        download_dir: Option<PathBuf>,

        #[arg(long)]
        token_file: Option<PathBuf>,

        #[arg(long, default_value_t = default_max_megabytes())]
        max_mb: u64,

        #[arg(long)]
        verbose: bool,
    },

    /// Install and start the macOS launchd receiver.
    InstallService {
        #[arg(long)]
        binary: Option<PathBuf>,

        #[arg(long, env = "HERDR_DOWNLOAD_PORT", default_value_t = DEFAULT_PORT)]
        port: u16,

        #[arg(long)]
        download_dir: Option<PathBuf>,

        #[arg(long)]
        token_file: Option<PathBuf>,
    },

    /// Report whether the macOS launchd receiver is running.
    ServiceStatus,

    /// Add the default prefix-d keybinding to a Herdr config file.
    ConfigureKeybinding {
        #[arg(long)]
        config: Option<PathBuf>,

        #[arg(long, default_value = "prefix+d")]
        key: String,
    },
}

#[derive(Clone, Debug, Args)]
struct TransferOptions {
    #[arg(long, env = "HERDR_DOWNLOAD_PORT", default_value_t = DEFAULT_PORT)]
    port: u16,

    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECONDS)]
    timeout: u64,

    #[arg(long, env = "HERDR_DOWNLOAD_SOCKET")]
    socket: Option<PathBuf>,

    #[arg(long, default_value_t = default_max_megabytes())]
    max_mb: u64,
}

fn default_max_megabytes() -> u64 {
    DEFAULT_MAX_BYTES / (1024 * 1024)
}

fn bytes_from_megabytes(value: u64) -> Result<u64> {
    if value == 0 {
        anyhow::bail!("--max-mb must be greater than zero");
    }
    value
        .checked_mul(1024 * 1024)
        .context("--max-mb is too large")
}

fn endpoint_for_context(options: &TransferOptions) -> Result<ReceiverEndpoint> {
    Ok(ReceiverEndpoint::Unix(match &options.socket {
        Some(path) => path.clone(),
        None => default_remote_socket_path()?,
    }))
}

fn endpoint_for_send(host: &str, options: &TransferOptions) -> ReceiverEndpoint {
    options
        .socket
        .clone()
        .map(ReceiverEndpoint::Unix)
        .unwrap_or_else(|| ReceiverEndpoint::Tcp {
            host: host.to_owned(),
            port: options.port,
        })
}

fn transfer(
    path: PathBuf,
    endpoint: ReceiverEndpoint,
    token_file: PathBuf,
    options: &TransferOptions,
) -> Result<serde_json::Value> {
    let token = read_token(&token_file)?;
    upload_file(
        &path,
        &endpoint,
        &token,
        options.timeout,
        bytes_from_megabytes(options.max_mb)?,
    )
    .with_context(|| format!("failed to transfer {}", path.display()))
}

fn run() -> Result<u8> {
    let cli = Cli::parse();

    match cli.command {
        Command::SendContext(options) => {
            let result: Result<()> = (|| {
                let path = resolve_path_from_context(&context_from_environment()?)?;
                let response = transfer(
                    path,
                    endpoint_for_context(&options)?,
                    sender_token_path()?,
                    &options,
                )?;
                notify_herdr(
                    "Herdr download complete",
                    &format!("Saved {}", response["path"].as_str().unwrap_or_default()),
                    "success",
                );
                println!("{}", serde_json::to_string(&response)?);
                Ok(())
            })();
            if let Err(error) = &result {
                notify_herdr("Herdr download failed", &format!("{error:#}"), "error");
            }
            result?;
        }
        Command::Send {
            path,
            host,
            token_file,
            transfer: options,
        } => {
            let token_file = match token_file {
                Some(path) => path,
                None => default_token_path()?,
            };
            let response = transfer(
                path,
                endpoint_for_send(&host, &options),
                token_file,
                &options,
            )?;
            println!("{}", serde_json::to_string(&response)?);
        }
        Command::InitToken { token_file } => {
            let path = match token_file {
                Some(path) => path,
                None => default_token_path()?,
            };
            ensure_token(&path)?;
            println!("{}", path.display());
        }
        Command::Serve {
            host,
            port,
            download_dir,
            token_file,
            max_mb,
            verbose,
        } => {
            let token_file = match token_file {
                Some(path) => path,
                None => default_token_path()?,
            };
            run_server(ServerConfig {
                host,
                port,
                destination: match download_dir {
                    Some(path) => path,
                    None => default_download_dir()?,
                },
                token: read_token(&token_file)?,
                max_bytes: bytes_from_megabytes(max_mb)?,
                verbose,
            })?;
        }
        Command::InstallService {
            binary,
            port,
            download_dir,
            token_file,
        } => {
            let download_dir = match download_dir {
                Some(path) => path,
                None => default_download_dir()?,
            };
            let token_file = match token_file {
                Some(path) => path,
                None => default_token_path()?,
            };
            let plist = install_service(binary.as_deref(), &token_file, &download_dir, port)?;
            println!("{}", plist.display());
        }
        Command::ServiceStatus => {
            let running = service_status()?;
            println!("{}", serde_json::to_string(&json!({"running": running}))?);
            return Ok(if running { 0 } else { 1 });
        }
        Command::ConfigureKeybinding { config, key } => {
            let config = match config {
                Some(path) => path,
                None => default_herdr_config_path()?,
            };
            let changed = configure_keybinding(&config, &key)?;
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "config": config,
                    "changed": changed,
                }))?
            );
        }
    }

    Ok(0)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("herdr-remote-download: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn max_megabytes_is_checked() {
        assert!(bytes_from_megabytes(0).is_err());
        assert!(bytes_from_megabytes(u64::MAX).is_err());
        assert_eq!(bytes_from_megabytes(2).unwrap(), 2 * 1024 * 1024);
    }
}
