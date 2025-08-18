use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;
use users::{get_user_by_name, get_group_by_name};
use nix::unistd::{setgid, setuid, Gid, Uid, chown, fork, ForkResult};
use nix::sys::wait::{waitpid, WaitStatus};
use std::os::unix::fs::PermissionsExt;
use tracing_subscriber::fmt::writer::MakeWriterExt;
use notify::{RecommendedWatcher, Watcher, RecursiveMode};
const SYS_PLUGINS_DIR: &str = "/etc/plugins";
const DEFAULT_LOG_DIRECTORY: &str = "/var/log/plugind";
const DEFAULT_SOCKET_PATH: &str = "/tmp/plugind.sock";

const LOG_FILE_PREFIX: &str = "daemon.log";
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Plugin {
    name: String,
    path: PathBuf,
    slot: String,
    event: Vec<String>,
    run_as: String,
    restart_on: String,
    enabled: bool,
    watch_path: Option<PathBuf>,
}

type PluginRegistry = Arc<Mutex<HashMap<String, Plugin>>>;
#[derive(Serialize, Deserialize, Debug)]
struct CommandRequest {
    home: Option<String>,
    current_user: Option<String>,
    daemon_command: DaemonCommand,
}

#[derive(Serialize, Deserialize, Debug)]
enum DaemonCommand {
    List,
    Info(String),
    Enable(String),
    Disable(String),
    Trigger(String, String, Option<String>),
    Reload,
    Status,
}

enum WatcherCommand {
    AddPath(PathBuf),
    RemovePath(PathBuf),
}

#[tokio::main]
async fn main() -> Result<()> {
    let log_directory = std::env::var("LOG_DIRECTORY").unwrap_or(DEFAULT_LOG_DIRECTORY.to_string());
    let socket_path = std::env::var("SOCKET_PATH").unwrap_or(DEFAULT_SOCKET_PATH.to_string());
    let file_appender = tracing_appender::rolling::daily(&log_directory, LOG_FILE_PREFIX);
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let subscriber = FmtSubscriber::builder()
        .with_writer(std::io::stdout.and(non_blocking))
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;
    info!("starting ObsidianOS plugind");
    let registry: PluginRegistry = Arc::new(Mutex::new(HashMap::new()));
    if Path::new(&socket_path).exists() {
        tokio::fs::remove_file(&socket_path).await?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    info!("listening on socket: {}", &socket_path);
    let path = Path::new(&socket_path);
    if let Some(group) = get_group_by_name("wheel") {
     let gid = Gid::from_raw(group.gid());
         if let Err(e) = chown(path, None, Some(gid)) {
             error!("failed to change socket group ownership to wheel: {}", e);
         }
     } else {
         error!("wheel group not found");
     }

     let permissions = std::fs::Permissions::from_mode(0o770);
     if let Err(e) = tokio::fs::set_permissions(path, permissions).await {
         error!("failed to set socket permissions: {}", e);
     }
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(100);
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(100);
    let registry_for_watcher = registry.clone();
    let mut watcher = RecommendedWatcher::new(move |res| {
        futures::executor::block_on(async {
            event_tx.send(res).await.unwrap();
        })
    }, notify::Config::default())?;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(res) = event_rx.recv() => {
                    match res {
                        Ok(event) => {
                            info!("file system event: {:?}", event);
                            let locked_registry = registry_for_watcher.lock().await;
                            for (_, plugin) in locked_registry.iter() {
                                if plugin.enabled {
                                    if let Some(watch_path) = &plugin.watch_path {
                                        for path in &event.paths {
                                            if path.starts_with(watch_path) {
                                                let event_name_to_pass = if *path == PathBuf::from("/sys/class/power_supply/BAT0/capacity") && event.kind.is_modify() {
                                                    "OnBatteryLevelChange".to_string()
                                                } else {
                                                    format!("{:?}", event.kind)
                                                };
                                                
                                                if plugin.event.contains(&event_name_to_pass) {
                                                    info!("triggering {} for event {}", plugin.name, event_name_to_pass);
                                                    if let Err(e) = execute_plugin(plugin, &event_name_to_pass, None, None).await {
                                                        error!("failed to execute plugin: {}", e);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                    }
                    Err(e) => {
                            error!("watcher event error: {}", e);
                        }
                    }
                }
                Some(command) = cmd_rx.recv() => {
                    match command {
                        WatcherCommand::AddPath(path) => {
                            if path.exists() {
                                if let Err(e) = watcher.watch(&path, RecursiveMode::Recursive) {
                                    error!("failed to watch path {:?}: {}", path, e);
                                }
                                info!("watching path: {:?}", path);
                            }
                        }
                        WatcherCommand::RemovePath(path) => {
                            if let Err(e) = watcher.unwatch(&path) {
                                error!("failed to unwatch path {:?}: {}", path, e);
                            }
                            info!("unwatching path: {:?}", path);
                        }
                    }
                }
            }
        }
    });

    let mut locked_registry = registry.lock().await;
    *locked_registry = load_plugins(None).await?;
    for (_, plugin) in locked_registry.iter() {
        if let Some(watch_path) = &plugin.watch_path {
            if watch_path.exists() {
                if let Err(e) = cmd_tx.send(WatcherCommand::AddPath(watch_path.clone())).await {
                    error!("failed to send add path command to watcher: {}", e);
                }
            }
        }
    }
    drop(locked_registry);
    let registry_for_battery = registry.clone();
    tokio::spawn(async move {
        let battery_capacity_path = PathBuf::from("/sys/class/power_supply/BAT0/capacity");
        let mut last_battery_level: Option<String> = None;
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            if battery_capacity_path.exists() {
                match tokio::fs::read_to_string(&battery_capacity_path).await {
                    Ok(current_level_str) => {
                        let current_level = current_level_str.trim().to_string();
                        if last_battery_level.is_none() || last_battery_level.as_ref().unwrap() != &current_level {
                            info!("battery level changed from {:?} to {}", last_battery_level, current_level);
                            last_battery_level = Some(current_level.clone());

                            let locked_registry = registry_for_battery.lock().await;
                            for (_, plugin) in locked_registry.iter() {
                                info!("Checking plugin: {} (enabled: {}, events: {:?}) for OnBatteryLevelChange", plugin.name, plugin.enabled, plugin.event);
                                if plugin.enabled && plugin.event.contains(&"OnBatteryLevelChange".to_string()) {
                                    info!("triggering {} for OnBatteryLevelChange", plugin.name);
                                    if let Err(e) = execute_plugin(plugin, "OnBatteryLevelChange", None, Some(current_level.clone())).await {
                                        error!("failed to execute plugin for OnBatteryLevelChange: {}", e);
                                    }
                                }
                            }
                        }
                    },
                    Err(e) => {
                        error!("failed to read battery capacity file: {}", e);
                    }
                }
            } else {
                
            }
        }
    });

    loop {
        let (stream, _) = listener.accept().await?;
        let registry = registry.clone();
        let cmd_tx_clone = cmd_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, registry, cmd_tx_clone).await {
                error!("client error: {}", e);
            }
        });
    }
}

async fn handle_client(mut stream: UnixStream, registry: PluginRegistry, cmd_tx: tokio::sync::mpsc::Sender<WatcherCommand>) -> Result<()> {
    let mut buffer = [0; 1024];
    let n = stream.read(&mut buffer).await?;
    let req: CommandRequest = serde_json::from_slice(&buffer[..n])?;
    let user_plugins_dir = req.home.map(|h| PathBuf::from(h).join(".config/plugins"));
    let ctl_user = req.current_user;
    let response = match req.daemon_command {
        DaemonCommand::List => {
            let mut locked_registry = registry.lock().await;
            *locked_registry = load_plugins(user_plugins_dir.as_deref()).await?;
            serde_json::to_vec(&*locked_registry)
        }
        DaemonCommand::Info(plugin_name) => {
            let mut locked_registry = registry.lock().await;
            *locked_registry = load_plugins(user_plugins_dir.as_deref()).await?;
            serde_json::to_vec(&locked_registry.get(&plugin_name))
        }
        DaemonCommand::Enable(plugin_name) => {
            let mut locked_registry = registry.lock().await;
            if let Some(plugin) = locked_registry.get_mut(&plugin_name) {
                plugin.enabled = true;
                info!("triggering {} for event OnEnabled", plugin.name);
                if let Err(e) = execute_plugin(plugin, "OnEnabled", ctl_user, None).await {
                    error!("failed to execute plugin for OnEnabled: {}", e);
                }
            }
            serde_json::to_vec(&"OK")
        }
        DaemonCommand::Disable(plugin_name) => {
            let mut locked_registry = registry.lock().await;
            if let Some(plugin) = locked_registry.get_mut(&plugin_name) {
                plugin.enabled = false;
                info!("triggering {} for event OnDisabled", plugin.name);
                if let Err(e) = execute_plugin(plugin, "OnDisabled", ctl_user, None).await {
                    error!("failed to execute plugin for OnDisabled: {}", e);
                }
            }
            serde_json::to_vec(&"OK")
        }
        DaemonCommand::Trigger(plugin_name, event, event_data) => {
            let locked_registry = registry.lock().await;
            if let Some(plugin) = locked_registry.get(&plugin_name) {
                if plugin.enabled {
                    info!("triggering {} for event {}", plugin.name, event);
                    execute_plugin(plugin, &event, ctl_user, event_data).await?;
                }
            }
            serde_json::to_vec(&"OK")
        }
        DaemonCommand::Reload => {
            let mut locked_registry = registry.lock().await;
            let old_plugins = locked_registry.clone();
            *locked_registry = load_plugins(user_plugins_dir.as_deref()).await?;
            let old_watch_paths: std::collections::HashSet<PathBuf> = old_plugins.values()
                .filter_map(|p| p.watch_path.clone())
                .collect();
            let new_watch_paths: std::collections::HashSet<PathBuf> = locked_registry.values()
                .filter_map(|p| p.watch_path.clone())
                .collect();

            for path in old_watch_paths.difference(&new_watch_paths) {
                if let Err(e) = cmd_tx.send(WatcherCommand::RemovePath(path.clone())).await {
                    error!("failed to send remove path command to watcher: {}", e);
                }
            }
            for path in new_watch_paths.difference(&old_watch_paths) {
                if let Err(e) = cmd_tx.send(WatcherCommand::AddPath(path.clone())).await {
                    error!("failed to send add path command to watcher: {}", e);
                }
            }
            serde_json::to_vec(&"OK")
        }
        DaemonCommand::Status => serde_json::to_vec(&"OK"),
    }?;

    stream.write_all(&response).await?;
    Ok(())
}

async fn load_plugins(user_plugins_dir: Option<&Path>) -> Result<HashMap<String, Plugin>> {
    let mut plugins = HashMap::new();
    load_plugins_from_dir(Path::new(SYS_PLUGINS_DIR), &mut plugins).await?;
    if let Some(dir) = user_plugins_dir {
        load_plugins_from_dir(dir, &mut plugins).await?;
    }
    Ok(plugins)
}

async fn load_plugins_from_dir(dir: &Path, plugins: &mut HashMap<String, Plugin>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            let manifest_path = path.join("manifest.plug");
            if manifest_path.exists() {
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if let Ok(plugin) = parse_manifest(&name, &manifest_path).await {
                    plugins.insert(name, plugin);
                }
            }
        }
    }
    Ok(()) 
}

async fn parse_manifest(name: &str, manifest_path: &Path) -> Result<Plugin> {
    let command = format!(
        "source {} && echo $SLOT && echo $EVENT && echo $RUN_AS && echo $RESTART_ON && echo $WATCH_PATH",
        manifest_path.to_string_lossy()
    );
    let output = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&command)
        .output()
        .await?;
    let output_str = String::from_utf8(output.stdout)?;
    let mut lines = output_str.lines();
    let slot = lines.next().unwrap_or("").to_string();
    let event_str = lines.next().unwrap_or("").to_string();
    let event: Vec<String> = event_str.split_whitespace().map(|s| s.to_string()).collect();
    let run_as = lines.next().unwrap_or("").to_string();
    let restart_on = lines.next().unwrap_or("").to_string();
    let watch_path = lines.next().and_then(|s| if s.is_empty() { None } else { Some(PathBuf::from(s)) });
    Ok(Plugin {
        name: name.to_string(),
        path: manifest_path.parent().unwrap().to_path_buf(),
        slot,
        event,
        run_as,
        restart_on,
        enabled: true,
        watch_path,
    })
}

async fn execute_plugin(plugin: &Plugin, event_name: &str, ctl_user: Option<String>, event_data: Option<String>) -> Result<()> {
    let slot_cmd = format!("readarray -t CURRENT_SLOT < <(obsidianctl status | grep \"Slot\" | awk '{{print $NF}}' | tail -n +2) && echo $CURRENT_SLOT > /tmp/slot.tmp");
    let output = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(slot_cmd)
        .output()
        .await?;

    if !output.status.success() {
        error!("failed to get current slot: {:?}", output);
        return Err(anyhow::anyhow!("failed to get current slot"));
    }

    let current_slot_raw = tokio::fs::read_to_string("/tmp/slot.tmp").await?;
    let current_slot = current_slot_raw.trim();
    if !plugin.slot.is_empty() && plugin.slot != "AB" && plugin.slot != current_slot {
        info!("plugin {} skipped due to slot mismatch. Required: {}, Current: {}", plugin.name, plugin.slot, current_slot);
        return Ok(());
    }

    let manifest_path = plugin.path.join("manifest.plug");
    let command = format!(
        "source {} && main",
        manifest_path.to_string_lossy()
    );

    let target_user = if plugin.run_as == "@" {
        ctl_user
    } else if plugin.run_as == "#" {
        Some("root".to_string())
    } else if !plugin.run_as.is_empty() {
        Some(plugin.run_as.clone())
    } else {
        None
    };

    match unsafe { fork() } {
        Ok(ForkResult::Parent { child, .. }) => {
            match waitpid(child, None)? {
                WaitStatus::Exited(_, status) => {
                    if status != 0 {
                        error!("plugin process exited with status: {}", status);
                        return Err(anyhow::anyhow!("plugin execution failed"));
                    }
                }
                WaitStatus::Signaled(_, signal, _) => {
                    error!("plugin process terminated by signal: {:?}", signal);
                    return Err(anyhow::anyhow!("plugin terminated by signal"));
                }
                _ => {
                    error!("plugin process did not exit normally");
                    return Err(anyhow::anyhow!("plugin process abnormal termination"));
                }
            }
        }
        Ok(ForkResult::Child) => {
            if let Some(user_name) = target_user {
                if let Some(user) = get_user_by_name(&user_name) {
                    let uid = Uid::from_raw(user.uid());
                    let gid = Gid::from_raw(user.primary_group_id());
                    if let Err(e) = setgid(gid) {
                        error!("child: failed to setgid to {}: {}", user_name, e);
                        std::process::exit(1);
                    }
                    if let Err(e) = setuid(uid) {
                        error!("child: failed to setuid to {}: {}", user_name, e);
                        std::process::exit(1);
                    }
                    info!("child: switched to user: {}", user_name);
                } else {
                    error!("child: user {} not found, running as current user", user_name);
                }
            }

            let mut cmd = std::process::Command::new("bash");
            cmd.arg("-c")
                .arg(&command)
                .env("EVENT_NAME", event_name);
            if let Some(data) = event_data {
                info!("Setting EVENT_RETURN to: {:?}", data);
                cmd.env("EVENT_RETURN", data.as_str());
            }

            let output = cmd.output().expect("Failed to execute command");
            let output_str = String::from_utf8_lossy(&output.stdout);
            info!("{}", output_str);
            if !output.status.success() {
                error!("child: command exited with non-zero status: {:?}, stderr: {}", output.status.code(), String::from_utf8_lossy(&output.stderr));
                std::process::exit(output.status.code().unwrap_or(1));
            }
            std::process::exit(0);
        }
        Err(e) => {
            error!("failed to fork: {}", e);
            return Err(anyhow::anyhow!("failed to fork process"));
        }
    }

    Ok(())
}

