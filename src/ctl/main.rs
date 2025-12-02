use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use cli_table::{print_stdout, Table, Cell, Style};
use users::get_current_username;
const DEFAULT_SOCKET_PATH: &str = "/tmp/plugind.sock";
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    cli_command: CliCommand,
    #[arg(long, global = true, default_value_t = false)]
    raw: bool,
}

#[derive(Subcommand, Debug, Serialize, Deserialize)]
enum CliCommand {
    List,
    Info { plugin_name: String },
    Enable { plugin_name: String },
    Disable { plugin_name: String },
    Trigger { plugin_name: String, event: String, output: Option<String> },
    Reload,
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let socket_path = std::env::var("SOCKET_PATH").unwrap_or(DEFAULT_SOCKET_PATH.to_string());
    let mut stream = UnixStream::connect(&socket_path).await?;
    let daemon_command_for_serialization = match cli.cli_command {
        CliCommand::List => DaemonCommandForSerialization::List,
        CliCommand::Info { plugin_name } => DaemonCommandForSerialization::Info(plugin_name),
        CliCommand::Enable { plugin_name } => DaemonCommandForSerialization::Enable(plugin_name),
        CliCommand::Disable { plugin_name } => DaemonCommandForSerialization::Disable(plugin_name),
        CliCommand::Trigger { plugin_name, event, output } => DaemonCommandForSerialization::Trigger(plugin_name, event, output),
        CliCommand::Reload => DaemonCommandForSerialization::Reload,
        CliCommand::Status => DaemonCommandForSerialization::Status,
    };

    let current_user = get_current_username()
        .and_then(|s| s.into_string().ok());

    let command = Command {
        home: std::env::var("HOME").ok(),
        current_user,
        daemon_command: daemon_command_for_serialization,
    };

    let serialized_command = serde_json::to_vec(&command)?;
    stream.write_all(&serialized_command).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    if cli.raw {
        println!("{}", String::from_utf8_lossy(&response));
    } else {
        format_output(&response)?;
    }

    Ok(())
}

fn format_output(response: &[u8]) -> Result<()> {
    let response_str = String::from_utf8_lossy(response);
    if response_str == "OK" {
        println!("OK");
        return Ok(());
    }

    if let Ok(plugins) = serde_json::from_slice::<HashMap<String, Plugin>>(response) {
        let table = plugins
            .into_iter()
            .map(|(_, p)| {
                vec![
                    p.name.cell(),
                    p.slot.cell(),
                    p.event.join(", ").cell(),
                    p.run_as.cell(),
                    p.restart_on.cell(),
                    p.enabled.to_string().cell(),
                ]
            })
            .collect::<Vec<_>>()
            .table()
            .title(vec![
                "Name".cell().bold(true),
                "Slot".cell().bold(true),
                "Event".cell().bold(true),
                "Run As".cell().bold(true),
                "Restart On".cell().bold(true),
                "Enabled".cell().bold(true),
            ]);

        print_stdout(table)?;
    } else if let Ok(Some(plugin)) = serde_json::from_slice::<Option<Plugin>>(response) {
        let table = vec![
            vec!["Name".cell().bold(true), plugin.name.cell()],
            vec!["Path".cell().bold(true), plugin.path.to_string_lossy().to_string().cell()],
            vec!["Slot".cell().bold(true), plugin.slot.cell()],
            vec!["Event".cell().bold(true), plugin.event.join(", ").cell()],
            vec!["Run As".cell().bold(true), plugin.run_as.cell()],
            vec!["Restart On".cell().bold(true), plugin.restart_on.cell()],
            vec!["Enabled".cell().bold(true), plugin.enabled.to_string().cell()],
        ]
        .table()
        .title(vec!["Field".cell().bold(true), "Value".cell().bold(true)]);

        print_stdout(table)?;
    }

    Ok(())
}

#[derive(Serialize, Deserialize, Debug)]
struct Command {
    home: Option<String>,
    current_user: Option<String>,
    daemon_command: DaemonCommandForSerialization,
}

#[derive(Serialize, Deserialize, Debug)]
enum DaemonCommandForSerialization {
    List,
    Info(String),
    Enable(String),
    Disable(String),
    Trigger(String, String, Option<String>),
    Reload,
    Status,
}

#[derive(Serialize, Deserialize, Debug)]
struct Plugin {
    name: String,
    path: std::path::PathBuf,
    slot: String,
    event: Vec<String>,
    run_as: String,
    restart_on: String,
    enabled: bool,
    watch_path: Option<std::path::PathBuf>,
}
