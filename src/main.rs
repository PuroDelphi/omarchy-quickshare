mod integration;
mod service;
mod share_selection;
mod ui_language;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use std::io::{self, Read, Write};
use std::path::PathBuf;

const BUS: &str = "io.omarchy.QuickShare";
const PATH: &str = "/io/omarchy/QuickShare";
const INTERFACE: &str = "io.omarchy.QuickShare1";

#[derive(Parser)]
#[command(version, about = "Quick Share para Omarchy")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the session service. Visibility starts disabled.
    Serve {
        #[arg(long, default_value_t = 47593, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        #[arg(long)]
        download_dir: Option<PathBuf>,
        #[arg(long)]
        no_bluetooth: bool,
        #[arg(long)]
        no_lan: bool,
        #[arg(long)]
        no_hotspot: bool,
    },
    Status,
    /// Print an initial snapshot and subsequent changes as NDJSON.
    Watch,
    /// Make discoverable for 1–600 seconds; 0 disables visibility.
    Visible {
        seconds: u32,
    },
    /// Permit or forbid temporary Wi-Fi interruption for direct transfers.
    NetworkConsent {
        #[arg(value_parser = ["on", "off"])]
        value: String,
    },
    /// Send files from a JSON path array on stdin to the selected peer.
    Send {
        peer: String,
    },
    /// Show an Android-native QR invitation for a JSON file-path array on stdin.
    QrSend,
    /// Withdraw only the identified pending QR invitation.
    QrStop {
        id: String,
    },
    /// Open the panel with a JSON path array on stdin; never start a transfer.
    Share,
    /// Consume a prepared selection and print its JSON path array.
    TakeShare {
        token: String,
    },
    Accept {
        id: String,
    },
    Reject {
        id: String,
    },
    Cancel {
        id: String,
    },
    /// Set the download directory from a JSON string on stdin.
    DownloadDir,
    /// Select local files or one directory through the desktop portal; output a JSON array.
    PickFiles {
        #[arg(long)]
        directory: bool,
    },
    /// Report actual local tools and radio capabilities (not Android interoperability).
    Doctor,
}

fn print_json_line(value: &str) -> Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "{value}")?;
    out.flush()?;
    Ok(())
}

const MAX_PRIVATE_INPUT_BYTES: u64 = 32 * 1024 * 1024;

fn read_private_json<T: serde::de::DeserializeOwned>(reader: impl Read) -> Result<T> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_PRIVATE_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("No se pudo leer la selección privada de stdin")?;
    if bytes.len() as u64 > MAX_PRIVATE_INPUT_BYTES {
        bail!("La selección privada supera el límite de 32 MiB");
    }
    serde_json::from_slice(&bytes).context("Se esperaba una selección JSON válida en stdin")
}

fn read_files(reader: impl Read) -> Result<Vec<String>> {
    let files: Vec<String> = read_private_json(reader)?;
    if files.is_empty() || files.len() > share_selection::MAX_FILES {
        bail!("Selecciona entre 1 y 1000 archivos");
    }
    Ok(files)
}

fn absolute_files(files: Vec<String>) -> Result<Vec<String>> {
    let mut directory = None;
    files.into_iter().map(|path| {
        if std::path::Path::new(&path).is_absolute() {
            return Ok(path);
        }
        if directory.is_none() {
            directory = Some(std::env::current_dir().context("No se encontró el directorio actual")?);
        }
        directory.as_ref().unwrap().join(path).into_os_string().into_string()
            .map_err(|_| anyhow::anyhow!("La ruta no puede representarse en D-Bus"))
    }).collect()
}

async fn share(files: Vec<String>) -> Result<()> {
    // This only preloads a selection. The daemon securely opens it when the
    // user authorizes Send/Show QR; do not resolve symlinks in the handoff.
    let paths = absolute_files(files)?;
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(&connection, BUS, PATH, INTERFACE).await?;
    let token: String = proxy.call("PrepareShare", &(paths,)).await?;
    let payload = serde_json::json!({ "share_token": token }).to_string();
    let summon_result = async {
        let output = tokio::process::Command::new("omarchy-shell")
            .args(["shell", "summon", "macondo.quickshare", &payload])
            .output()
            .await
            .context("No se pudo ejecutar omarchy-shell para abrir Quick Share")?;
        if !output.status.success() {
            bail!(
                "No se pudo abrir el panel Quick Share: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let response = std::str::from_utf8(&output.stdout)
            .context("Respuesta no válida del shell Omarchy")?
            .trim();
        if response != "ok" {
            bail!(
                "El shell no abrió Quick Share (respuesta: {response:?}); comprueba que el plugin esté instalado y habilitado"
            );
        }
        Ok(())
    }
    .await;
    if summon_result.is_err() {
        // Discard the private selection on every shell failure, including spawn/parse failures.
        // The panel may already have consumed it, so a missing token is harmless here.
        let _: zbus::Result<Vec<String>> = proxy.call("TakeShare", &(token,)).await;
    }
    summon_result
}

#[derive(serde::Deserialize)]
struct SnapshotOrder<'a> {
    #[serde(borrow)]
    instance_id: &'a str,
    revision: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Serve {
        port,
        download_dir,
        no_bluetooth,
        no_lan,
        no_hotspot,
    } = cli.command
    {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn".into()),
            )
            .with_writer(io::stderr)
            .init();
        return service::serve(
            port,
            download_dir,
            rqs_lib::EngineOptions {
                bluetooth: !no_bluetooth,
                lan: !no_lan,
                hotspot: !no_hotspot,
            },
        )
        .await;
    }
    match cli.command {
        Command::Share => share(read_files(io::stdin().lock())?).await?,
        Command::PickFiles { directory } => {
            let selection = ashpd::desktop::file_chooser::SelectedFiles::open_file()
                .title(if directory {
                    "Quick Share — Download folder"
                } else {
                    "Compartir con Quick Share"
                })
                .directory(directory)
                .multiple(!directory)
                .modal(false)
                .send()
                .await
                .and_then(|request| request.response());
            let response = match selection {
                Ok(response) => response,
                Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {
                    print_json_line("[]")?;
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            let paths: Vec<PathBuf> = response
                .uris()
                .iter()
                .map(|uri| {
                    url::Url::parse(uri.as_str())?
                        .to_file_path()
                        .map_err(|_| anyhow::anyhow!("El portal devolvió una URI no local"))
                })
                .collect::<Result<_>>()?;
            print_json_line(&serde_json::to_string(&paths)?)?;
        }
        Command::Doctor => service::doctor().await?,
        other => {
            let connection = zbus::Connection::session().await?;
            let proxy = zbus::Proxy::new(&connection, BUS, PATH, INTERFACE).await?;
            match other {
                Command::Status => {
                    let snapshot: String = proxy
                        .call("Snapshot", &())
                        .await
                        .context("El servicio Quick Share no está disponible")?;
                    print_json_line(&snapshot)?;
                }
                Command::Watch => {
                    // Subscribe before taking the snapshot so an update cannot be missed.
                    let mut signals = proxy.receive_signal("Changed").await?;
                    let mut owners = proxy.receive_owner_changed().await?;
                    let snapshot: String = proxy.call("Snapshot", &()).await?;
                    let order: SnapshotOrder<'_> = serde_json::from_str(&snapshot)?;
                    let instance = order.instance_id.to_owned();
                    let mut revision = order.revision;
                    print_json_line(&snapshot)?;
                    loop {
                        tokio::select! {
                            signal = signals.next() => {
                                let signal = signal.context("La conexión con el servicio terminó")?;
                                let (snapshot,): (String,) = signal.body().deserialize()?;
                                let order: SnapshotOrder<'_> = serde_json::from_str(&snapshot)?;
                                if order.instance_id != instance { bail!("El servicio Quick Share se reinició"); }
                                if order.revision > revision {
                                    revision = order.revision;
                                    print_json_line(&snapshot)?;
                                }
                            }
                            _ = owners.next() => bail!("El servicio Quick Share se detuvo o reinició"),
                        }
                    }
                }
                Command::Visible { seconds } => {
                    let _: () = proxy.call("SetVisibility", &(seconds,)).await?;
                }
                Command::NetworkConsent { value } => {
                    let _: () = proxy.call("SetNetworkConsent", &(value == "on",)).await?;
                }
                Command::Send { peer } => {
                    let paths = absolute_files(read_files(io::stdin().lock())?)?;
                    let id: String = proxy.call("Send", &(peer, paths)).await?;
                    println!("{id}");
                }
                Command::QrSend => {
                    let paths = absolute_files(read_files(io::stdin().lock())?)?;
                    let invitation: String = proxy.call("StartQrSend", &(paths,)).await?;
                    print_json_line(&invitation)?;
                }
                Command::QrStop { id } => {
                    let _: () = proxy.call("StopQrSend", &(id,)).await?;
                }
                Command::TakeShare { token } => {
                    let paths: Vec<String> = proxy.call("TakeShare", &(token,)).await?;
                    print_json_line(&serde_json::to_string(&paths)?)?;
                }
                Command::Accept { id } => {
                    let _: () = proxy.call("Respond", &(id, true)).await?;
                }
                Command::Reject { id } => {
                    let _: () = proxy.call("Respond", &(id, false)).await?;
                }
                Command::Cancel { id } => {
                    let _: () = proxy.call("Cancel", &(id,)).await?;
                }
                Command::DownloadDir => {
                    let path: String = read_private_json(io::stdin().lock())?;
                    let path = std::fs::canonicalize(path)?;
                    let path = path.to_str().context("Ruta no representable en D-Bus")?;
                    let _: () = proxy.call("SetDownloadDirectory", &(path,)).await?;
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_file_input_preserves_json_paths_and_enforces_count_and_type() {
        let files = vec![
            "/private/quote\" and ü".to_owned(),
            "/private/new\nline".to_owned(),
        ];
        let json = serde_json::to_vec(&files).unwrap();
        assert_eq!(read_files(json.as_slice()).unwrap(), files);
        for input in [
            "",
            "[]",
            "null",
            "{}",
            "[1]",
            "[\"file\"",
            "\"/private/file\"",
        ] {
            assert!(read_files(input.as_bytes()).is_err(), "{input:?}");
        }
        let maximum = vec!["/private/file"; share_selection::MAX_FILES];
        assert_eq!(
            read_files(serde_json::to_vec(&maximum).unwrap().as_slice())
                .unwrap()
                .len(),
            share_selection::MAX_FILES
        );
        let excessive = vec!["/private/file"; share_selection::MAX_FILES + 1];
        assert!(read_files(serde_json::to_vec(&excessive).unwrap().as_slice()).is_err());
    }

    #[test]
    fn private_input_has_an_exact_byte_limit_without_truncating_json() {
        let padding = io::repeat(b' ').take(MAX_PRIVATE_INPUT_BYTES - 2);
        let exact = io::Cursor::new(b"[]").chain(padding);
        assert!(read_private_json::<Vec<String>>(exact).unwrap().is_empty());
        let oversized =
            io::Cursor::new(b"[]").chain(io::repeat(b' ').take(MAX_PRIVATE_INPUT_BYTES - 1));
        assert!(read_private_json::<Vec<String>>(oversized).is_err());
        assert!(read_private_json::<String>(io::Cursor::new(b"\"/private/dir\"")).is_ok());
        assert!(read_private_json::<String>(io::Cursor::new(b"[\"/private/dir\"]")).is_err());
        assert!(read_private_json::<Vec<String>>(io::Cursor::new(b"[\"\xff\"]")).is_err());
    }

    #[test]
    fn private_paths_are_not_accepted_as_cli_arguments() {
        for arguments in [
            vec!["quickshare", "share", "/private/file"],
            vec!["quickshare", "send", "peer", "/private/file"],
            vec!["quickshare", "download-dir", "/private/directory"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        assert!(Cli::try_parse_from(["quickshare", "share"]).is_ok());
        assert!(Cli::try_parse_from(["quickshare", "send", "peer"]).is_ok());
        assert!(Cli::try_parse_from(["quickshare", "download-dir"]).is_ok());
        assert!(Cli::try_parse_from(["quickshare", "take-share", "opaque-token"]).is_ok());
    }
}
