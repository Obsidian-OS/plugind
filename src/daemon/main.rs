use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{error, info, warn, Level};
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

#[derive(Debug, Clone, Default)]
struct SystemState {
    battery_level: Option<u8>,
    battery_status: Option<String>,
    ac_power: Option<bool>,
    network_interfaces: HashMap<String, NetworkInterfaceState>,
    default_route: Option<String>,
    display_brightness: Option<u8>,
    displays_connected: Vec<String>,
    audio_volume: Option<u8>,
    audio_muted: Option<bool>,
    default_sink: Option<String>,
    cpu_usage: Option<f32>,
    memory_usage: Option<f32>,
    load_average: Option<(f32, f32, f32)>,
    active_user: Option<String>,
    session_locked: Option<bool>,
    timezone: Option<String>,
    usb_devices: Vec<UsbDevice>,
    bluetooth_powered: Option<bool>,
    bluetooth_devices: Vec<BluetoothDevice>,
    disk_usage: HashMap<String, DiskUsage>,
}

#[derive(Debug, Clone, PartialEq)]
struct NetworkInterfaceState {
    name: String,
    is_up: bool,
    ip_addresses: Vec<String>,
    interface_type: String,
    ssid: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct UsbDevice {
    vendor_id: String,
    product_id: String,
    name: String,
    path: String,
}

#[derive(Debug, Clone, PartialEq)]
struct BluetoothDevice {
    address: String,
    name: String,
    connected: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct DiskUsage {
    mount_point: String,
    total_bytes: u64,
    used_bytes: u64,
    available_bytes: u64,
    usage_percent: u8,
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
    let system_state: Arc<Mutex<SystemState>> = Arc::new(Mutex::new(SystemState::default()));
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
            let _ = event_tx.send(res).await;
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
                                                let event_name = match event.kind {
                                                    notify::EventKind::Create(_) => "OnFileCreate",
                                                    notify::EventKind::Modify(_) => "OnFileModify",
                                                    notify::EventKind::Remove(_) => "OnFileDelete",
                                                    notify::EventKind::Access(_) => "OnFileAccess",
                                                    _ => "OnFileChange",
                                                };

                                                if plugin.event.contains(&event_name.to_string()) {
                                                    info!("triggering {} for event {}", plugin.name, event_name);
                                                    let event_data = path.to_string_lossy().to_string();
                                                    if let Err(e) = execute_plugin(plugin, event_name, None, Some(event_data)).await {
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
    trigger_event_for_all(&registry, "OnDaemonStart", None).await;
    let registry_for_battery = registry.clone();
    let state_for_battery = system_state.clone();
    tokio::spawn(async move {
        let battery_capacity_path = PathBuf::from("/sys/class/power_supply/BAT0/capacity");
        let battery_status_path = PathBuf::from("/sys/class/power_supply/BAT0/status");
        let ac_online_path = PathBuf::from("/sys/class/power_supply/AC/online");
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let mut state = state_for_battery.lock().await;
            if battery_capacity_path.exists() {
                if let Ok(level_str) = tokio::fs::read_to_string(&battery_capacity_path).await {
                    if let Ok(level) = level_str.trim().parse::<u8>() {
                        let old_level = state.battery_level;
                        state.battery_level = Some(level);
                        if old_level != Some(level) {
                            drop(state);
                            info!("battery level changed: {:?} -> {}", old_level, level);
                            trigger_event_for_all(&registry_for_battery, "OnBatteryLevelChange", Some(level.to_string())).await;
                            if level <= 5 && old_level.map_or(true, |o| o > 5) {
                                trigger_event_for_all(&registry_for_battery, "OnBatteryCritical", Some(level.to_string())).await;
                            } else if level <= 15 && old_level.map_or(true, |o| o > 15) {
                                trigger_event_for_all(&registry_for_battery, "OnBatteryLow", Some(level.to_string())).await;
                            } else if level >= 100 && old_level.map_or(true, |o| o < 100) {
                                trigger_event_for_all(&registry_for_battery, "OnBatteryFull", None).await;
                            }

                            state = state_for_battery.lock().await;
                        }
                    }
                }
            }

            if battery_status_path.exists() {
                if let Ok(status_str) = tokio::fs::read_to_string(&battery_status_path).await {
                    let status = status_str.trim().to_string();
                    let old_status = state.battery_status.clone();
                    if old_status.as_ref() != Some(&status) {
                        state.battery_status = Some(status.clone());
                        drop(state);
                        info!("battery status changed: {:?} -> {}", old_status, status);
                        match status.as_str() {
                            "Charging" => {
                                trigger_event_for_all(&registry_for_battery, "OnBatteryCharging", None).await;
                            }
                            "Discharging" => {
                                trigger_event_for_all(&registry_for_battery, "OnBatteryDischarging", None).await;
                            }
                            "Full" => {
                                trigger_event_for_all(&registry_for_battery, "OnBatteryFull", None).await;
                            }
                            _ => {}
                        }

                        state = state_for_battery.lock().await;
                    }
                }
            }

            if ac_online_path.exists() {
                if let Ok(ac_str) = tokio::fs::read_to_string(&ac_online_path).await {
                    let ac_connected = ac_str.trim() == "1";
                    let old_ac = state.ac_power;
                    if old_ac != Some(ac_connected) {
                        state.ac_power = Some(ac_connected);
                        drop(state);
                        info!("AC power changed: {:?} -> {}", old_ac, ac_connected);
                        if ac_connected {
                            trigger_event_for_all(&registry_for_battery, "OnACConnected", None).await;
                        } else {
                            trigger_event_for_all(&registry_for_battery, "OnACDisconnected", None).await;
                        }

                        state = state_for_battery.lock().await;
                    }
                }
            }

            drop(state);
        }
    });

    let registry_for_network = registry.clone();
    let state_for_network = system_state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            let mut state = state_for_network.lock().await;
            let old_interfaces = state.network_interfaces.clone();
            let new_interfaces = get_network_interfaces().await;
            for (name, new_iface) in &new_interfaces {
                let old_iface = old_interfaces.get(name);
                match old_iface {
                    None => {
                        info!("network interface appeared: {}", name);
                        let event_data = serde_json::json!({
                            "interface": name,
                            "type": new_iface.interface_type,
                            "is_up": new_iface.is_up
                        }).to_string();
                        drop(state);
                        trigger_event_for_all(&registry_for_network, "OnNetworkInterfaceAdded", Some(event_data)).await;
                        state = state_for_network.lock().await;
                    }
                    Some(old) => {
                        if old.is_up != new_iface.is_up {
                            info!("network interface {} state changed: {} -> {}", name, old.is_up, new_iface.is_up);
                            let event_data = serde_json::json!({
                                "interface": name,
                                "type": new_iface.interface_type,
                                "is_up": new_iface.is_up
                            }).to_string();
                            drop(state);
                            if new_iface.is_up {
                                trigger_event_for_all(&registry_for_network, "OnNetworkUp", Some(event_data)).await;
                            } else {
                                trigger_event_for_all(&registry_for_network, "OnNetworkDown", Some(event_data)).await;
                            }

                            state = state_for_network.lock().await;
                        }

                        if new_iface.interface_type == "wifi" && old.ssid != new_iface.ssid {
                            if let Some(ssid) = &new_iface.ssid {
                                info!("WiFi connected to: {}", ssid);
                                let event_data = serde_json::json!({
                                    "interface": name,
                                    "ssid": ssid
                                }).to_string();
                                drop(state);
                                trigger_event_for_all(&registry_for_network, "OnWifiConnected", Some(event_data)).await;
                                state = state_for_network.lock().await;
                            } else if old.ssid.is_some() {
                                info!("WiFi disconnected from: {:?}", old.ssid);
                                drop(state);
                                trigger_event_for_all(&registry_for_network, "OnWifiDisconnected", Some(name.clone())).await;
                                state = state_for_network.lock().await;
                            }
                        }

                        if old.ip_addresses != new_iface.ip_addresses {
                            info!("IP addresses changed on {}: {:?} -> {:?}", name, old.ip_addresses, new_iface.ip_addresses);
                            let event_data = serde_json::json!({
                                "interface": name,
                                "old_ips": old.ip_addresses,
                                "new_ips": new_iface.ip_addresses
                            }).to_string();
                            drop(state);
                            trigger_event_for_all(&registry_for_network, "OnIPAddressChange", Some(event_data)).await;
                            state = state_for_network.lock().await;
                        }
                    }
                }
            }

            for (name, _) in &old_interfaces {
                if !new_interfaces.contains_key(name) {
                    info!("network interface removed: {}", name);
                    drop(state);
                    trigger_event_for_all(&registry_for_network, "OnNetworkInterfaceRemoved", Some(name.clone())).await;
                    state = state_for_network.lock().await;
                }
            }

            state.network_interfaces = new_interfaces;
            drop(state);
        }
    });

    let registry_for_usb = registry.clone();
    let state_for_usb = system_state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            let mut state = state_for_usb.lock().await;
            let old_devices = state.usb_devices.clone();
            let new_devices = get_usb_devices().await;
            for device in &new_devices {
                if !old_devices.contains(device) {
                    info!("USB device connected: {} ({}:{})", device.name, device.vendor_id, device.product_id);
                    let event_data = serde_json::json!({
                        "vendor_id": device.vendor_id,
                        "product_id": device.product_id,
                        "name": device.name,
                        "path": device.path
                    }).to_string();
                    drop(state);
                    trigger_event_for_all(&registry_for_usb, "OnUSBConnected", Some(event_data)).await;
                    state = state_for_usb.lock().await;
                }
            }

            for device in &old_devices {
                if !new_devices.contains(device) {
                    info!("USB device disconnected: {} ({}:{})", device.name, device.vendor_id, device.product_id);
                    let event_data = serde_json::json!({
                        "vendor_id": device.vendor_id,
                        "product_id": device.product_id,
                        "name": device.name,
                        "path": device.path
                    }).to_string();
                    drop(state);
                    trigger_event_for_all(&registry_for_usb, "OnUSBDisconnected", Some(event_data)).await;
                    state = state_for_usb.lock().await;
                }
            }

            state.usb_devices = new_devices;
            drop(state);
        }
    });

    let registry_for_display = registry.clone();
    let state_for_display = system_state.clone();
    tokio::spawn(async move {
        let backlight_path = PathBuf::from("/sys/class/backlight");
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            let mut state = state_for_display.lock().await;
            if backlight_path.exists() {
                if let Ok(mut entries) = tokio::fs::read_dir(&backlight_path).await {
                    if let Ok(Some(entry)) = entries.next_entry().await {
                        let brightness_path = entry.path().join("brightness");
                        let max_brightness_path = entry.path().join("max_brightness");
                        if let (Ok(current), Ok(max)) = (
                            tokio::fs::read_to_string(&brightness_path).await,
                            tokio::fs::read_to_string(&max_brightness_path).await
                        ) {
                            if let (Ok(current_val), Ok(max_val)) = (
                                current.trim().parse::<u64>(),
                                max.trim().parse::<u64>()
                            ) {
                                let brightness_percent = ((current_val * 100) / max_val) as u8;
                                let old_brightness = state.display_brightness;
                                if old_brightness != Some(brightness_percent) {
                                    state.display_brightness = Some(brightness_percent);
                                    drop(state);
                                    info!("display brightness changed: {:?} -> {}%", old_brightness, brightness_percent);
                                    trigger_event_for_all(&registry_for_display, "OnBrightnessChange", Some(brightness_percent.to_string())).await;
                                    state = state_for_display.lock().await;
                                }
                            }
                        }
                    }
                }
            }

            let old_displays = state.displays_connected.clone();
            let new_displays = get_connected_displays().await;
            if old_displays != new_displays {
                for disp in &new_displays {
                    if !old_displays.contains(disp) {
                        info!("display connected: {}", disp);
                        drop(state);
                        trigger_event_for_all(&registry_for_display, "OnDisplayConnected", Some(disp.clone())).await;
                        state = state_for_display.lock().await;
                    }
                }

                for disp in &old_displays {
                    if !new_displays.contains(disp) {
                        info!("display disconnected: {}", disp);
                        drop(state);
                        trigger_event_for_all(&registry_for_display, "OnDisplayDisconnected", Some(disp.clone())).await;
                        state = state_for_display.lock().await;
                    }
                }

                state.displays_connected = new_displays;
            }

            drop(state);
        }
    });

    let registry_for_resources = registry.clone();
    let state_for_resources = system_state.clone();
    tokio::spawn(async move {
        let mut last_cpu_idle: Option<u64> = None;
        let mut last_cpu_total: Option<u64> = None;
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
            let mut state = state_for_resources.lock().await;
            if let Ok(stat) = tokio::fs::read_to_string("/proc/stat").await {
                if let Some(cpu_line) = stat.lines().next() {
                    let parts: Vec<u64> = cpu_line
                        .split_whitespace()
                        .skip(1)
                        .filter_map(|s| s.parse().ok())
                        .collect();

                    if parts.len() >= 4 {
                        let idle = parts[3];
                        let total: u64 = parts.iter().sum();
                        if let (Some(last_idle), Some(last_total)) = (last_cpu_idle, last_cpu_total) {
                            let idle_diff = idle.saturating_sub(last_idle);
                            let total_diff = total.saturating_sub(last_total);
                            if total_diff > 0 {
                                let cpu_usage = 100.0 * (1.0 - (idle_diff as f32 / total_diff as f32));
                                state.cpu_usage = Some(cpu_usage);
                                if cpu_usage > 90.0 {
                                    drop(state);
                                    trigger_event_for_all(&registry_for_resources, "OnHighCPU", Some(format!("{:.1}", cpu_usage))).await;
                                    state = state_for_resources.lock().await;
                                }
                            }
                        }

                        last_cpu_idle = Some(idle);
                        last_cpu_total = Some(total);
                    }
                }
            }

            if let Ok(meminfo) = tokio::fs::read_to_string("/proc/meminfo").await {
                let mut total: Option<u64> = None;
                let mut available: Option<u64> = None;
                for line in meminfo.lines() {
                    if line.starts_with("MemTotal:") {
                        total = line.split_whitespace().nth(1).and_then(|s| s.parse().ok());
                    } else if line.starts_with("MemAvailable:") {
                        available = line.split_whitespace().nth(1).and_then(|s| s.parse().ok());
                    }
                }

                if let (Some(t), Some(a)) = (total, available) {
                    let usage = 100.0 * (1.0 - (a as f32 / t as f32));
                    let old_usage = state.memory_usage;
                    state.memory_usage = Some(usage);
                    if usage > 90.0 && old_usage.map_or(true, |o| o <= 90.0) {
                        drop(state);
                        trigger_event_for_all(&registry_for_resources, "OnHighMemory", Some(format!("{:.1}", usage))).await;
                        state = state_for_resources.lock().await;
                    }
                }
            }

            if let Ok(loadavg) = tokio::fs::read_to_string("/proc/loadavg").await {
                let parts: Vec<f32> = loadavg
                    .split_whitespace()
                    .take(3)
                    .filter_map(|s| s.parse().ok())
                    .collect();

                if parts.len() >= 3 {
                    state.load_average = Some((parts[0], parts[1], parts[2]));
                }
            }

            drop(state);
        }
    });

    let registry_for_disk = registry.clone();
    let state_for_disk = system_state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            let mut state = state_for_disk.lock().await;
            let old_disk_usage = state.disk_usage.clone();
            let new_disk_usage = get_disk_usage().await;
            for (mount, usage) in &new_disk_usage {
                if usage.usage_percent >= 90 {
                    if let Some(old) = old_disk_usage.get(mount) {
                        if old.usage_percent < 90 {
                            info!("disk {} usage critical: {}%", mount, usage.usage_percent);
                            let event_data = serde_json::json!({
                                "mount_point": mount,
                                "usage_percent": usage.usage_percent,
                                "available_bytes": usage.available_bytes
                            }).to_string();
                            drop(state);
                            trigger_event_for_all(&registry_for_disk, "OnDiskSpaceLow", Some(event_data)).await;
                            state = state_for_disk.lock().await;
                        }
                    }
                }
            }

            state.disk_usage = new_disk_usage;
            drop(state);
        }
    });

    let registry_for_bluetooth = registry.clone();
    let state_for_bluetooth = system_state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let mut state = state_for_bluetooth.lock().await;
            let old_devices = state.bluetooth_devices.clone();
            let new_devices = get_bluetooth_devices().await;
            for device in &new_devices {
                let old_device = old_devices.iter().find(|d| d.address == device.address);
                match old_device {
                    None if device.connected => {
                        info!("bluetooth device connected: {} ({})", device.name, device.address);
                        let event_data = serde_json::json!({
                            "address": device.address,
                            "name": device.name
                        }).to_string();
                        drop(state);
                        trigger_event_for_all(&registry_for_bluetooth, "OnBluetoothDeviceConnected", Some(event_data)).await;
                        state = state_for_bluetooth.lock().await;
                    }
                    Some(old) if !old.connected && device.connected => {
                        info!("bluetooth device connected: {} ({})", device.name, device.address);
                        let event_data = serde_json::json!({
                            "address": device.address,
                            "name": device.name
                        }).to_string();
                        drop(state);
                        trigger_event_for_all(&registry_for_bluetooth, "OnBluetoothDeviceConnected", Some(event_data)).await;
                        state = state_for_bluetooth.lock().await;
                    }
                    Some(old) if old.connected && !device.connected => {
                        info!("bluetooth device disconnected: {} ({})", device.name, device.address);
                        let event_data = serde_json::json!({
                            "address": device.address,
                            "name": device.name
                        }).to_string();
                        drop(state);
                        trigger_event_for_all(&registry_for_bluetooth, "OnBluetoothDeviceDisconnected", Some(event_data)).await;
                        state = state_for_bluetooth.lock().await;
                    }
                    _ => {}
                }
            }

            state.bluetooth_devices = new_devices;
            drop(state);
        }
    });

    let registry_for_lid = registry.clone();
    tokio::spawn(async move {
        let lid_state_path = PathBuf::from("/proc/acpi/button/lid/LID0/state");
        let mut last_lid_state: Option<String> = None;
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            if lid_state_path.exists() {
                if let Ok(content) = tokio::fs::read_to_string(&lid_state_path).await {
                    let state = if content.contains("open") {
                        "open".to_string()
                    } else if content.contains("closed") {
                        "closed".to_string()
                    } else {
                        continue;
                    };

                    if last_lid_state.as_ref() != Some(&state) {
                        info!("lid state changed: {:?} -> {}", last_lid_state, state);
                        if state == "open" {
                            trigger_event_for_all(&registry_for_lid, "OnLidOpen", None).await;
                        } else {
                            trigger_event_for_all(&registry_for_lid, "OnLidClose", None).await;
                        }

                        last_lid_state = Some(state);
                    }
                }
            }
        }
    });

    let registry_for_tz = registry.clone();
    let state_for_tz = system_state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            let mut state = state_for_tz.lock().await;
            if let Ok(tz) = tokio::fs::read_link("/etc/localtime").await {
                let tz_str = tz.to_string_lossy().to_string();
                if state.timezone.as_ref() != Some(&tz_str) {
                    if state.timezone.is_some() {
                        info!("timezone changed to: {}", tz_str);
                        drop(state);
                        trigger_event_for_all(&registry_for_tz, "OnTimezoneChange", Some(tz_str.clone())).await;
                        state = state_for_tz.lock().await;
                    }
                    state.timezone = Some(tz_str);
                }
            }

            drop(state);
        }
    });

    let registry_for_session = registry.clone();
    let state_for_session = system_state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let mut state = state_for_session.lock().await;
            let output = tokio::process::Command::new("loginctl")
                .args(["show-session", "self", "-p", "LockedHint"])
                .output()
                .await;

            if let Ok(output) = output {
                let output_str = String::from_utf8_lossy(&output.stdout);
                let is_locked = output_str.contains("yes");
                let was_locked = state.session_locked;
                if was_locked != Some(is_locked) {
                    state.session_locked = Some(is_locked);
                    drop(state);

                    if is_locked {
                        info!("session locked");
                        trigger_event_for_all(&registry_for_session, "OnSessionLock", None).await;
                    } else if was_locked == Some(true) {
                        info!("session unlocked");
                        trigger_event_for_all(&registry_for_session, "OnSessionUnlock", None).await;
                    }

                    state = state_for_session.lock().await;
                }
            }

            drop(state);
        }
    });

    let registry_for_power = registry.clone();
    tokio::spawn(async move {
        let mut last_uptime: Option<f64> = None;
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            if let Ok(uptime_str) = tokio::fs::read_to_string("/proc/uptime").await {
                if let Some(uptime) = uptime_str.split_whitespace().next().and_then(|s| s.parse::<f64>().ok()) {
                    if let Some(last) = last_uptime {
                        let expected_diff = 5.0;
                        let actual_diff = uptime - last;
                        if actual_diff > expected_diff * 3.0 {
                            info!("system resumed from suspend (uptime gap: {:.1}s)", actual_diff);
                            trigger_event_for_all(&registry_for_power, "OnResume", None).await;
                        }
                    }
                    last_uptime = Some(uptime);
                }
            }
        }
    });

    let registry_for_audio = registry.clone();
    let state_for_audio = system_state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            let mut state = state_for_audio.lock().await;
            let output = tokio::process::Command::new("pactl")
                .args(["get-sink-volume", "@DEFAULT_SINK@"])
                .output()
                .await;

            if let Ok(output) = output {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(percent_str) = stdout.split('/').nth(1) {
                    if let Ok(volume) = percent_str.trim().trim_end_matches('%').parse::<u8>() {
                        let old_volume = state.audio_volume;
                        if old_volume != Some(volume) {
                            state.audio_volume = Some(volume);
                            drop(state);
                            info!("audio volume changed: {:?} -> {}%", old_volume, volume);
                            trigger_event_for_all(&registry_for_audio, "OnVolumeChange", Some(volume.to_string())).await;
                            state = state_for_audio.lock().await;
                        }
                    }
                }
            }

            let output = tokio::process::Command::new("pactl")
                .args(["get-sink-mute", "@DEFAULT_SINK@"])
                .output()
                .await;

            if let Ok(output) = output {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let is_muted = stdout.contains("yes");
                let was_muted = state.audio_muted;

                if was_muted != Some(is_muted) {
                    state.audio_muted = Some(is_muted);
                    drop(state);

                    if is_muted {
                        info!("audio muted");
                        trigger_event_for_all(&registry_for_audio, "OnMute", None).await;
                    } else if was_muted == Some(true) {
                        info!("audio unmuted");
                        trigger_event_for_all(&registry_for_audio, "OnUnmute", None).await;
                    }

                    state = state_for_audio.lock().await;
                }
            }

            let output = tokio::process::Command::new("pactl")
                .args(["get-default-sink"])
                .output()
                .await;

            if let Ok(output) = output {
                let sink = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !sink.is_empty() && state.default_sink.as_ref() != Some(&sink) {
                    let old_sink = state.default_sink.clone();
                    state.default_sink = Some(sink.clone());
                    if old_sink.is_some() {
                        drop(state);
                        info!("default audio sink changed: {:?} -> {}", old_sink, sink);
                        trigger_event_for_all(&registry_for_audio, "OnAudioOutputChange", Some(sink)).await;
                        state = state_for_audio.lock().await;
                    }
                }
            }

            drop(state);
        }
    });

    let registry_for_bt_power = registry.clone();
    let state_for_bt_power = system_state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let mut state = state_for_bt_power.lock().await;
            let output = tokio::process::Command::new("bluetoothctl")
                .args(["show"])
                .output()
                .await;

            if let Ok(output) = output {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let is_powered = stdout.lines()
                    .find(|l| l.contains("Powered:"))
                    .map(|l| l.contains("yes"))
                    .unwrap_or(false);

                let was_powered = state.bluetooth_powered;
                if was_powered != Some(is_powered) {
                    state.bluetooth_powered = Some(is_powered);
                    drop(state);
                    if is_powered {
                        info!("bluetooth powered on");
                        trigger_event_for_all(&registry_for_bt_power, "OnBluetoothOn", None).await;
                    } else if was_powered == Some(true) {
                        info!("bluetooth powered off");
                        trigger_event_for_all(&registry_for_bt_power, "OnBluetoothOff", None).await;
                    }

                    state = state_for_bt_power.lock().await;
                }
            }

            drop(state);
        }
    });

    let registry_for_user = registry.clone();
    let state_for_user = system_state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let mut state = state_for_user.lock().await;
            let output = tokio::process::Command::new("loginctl")
                .args(["list-sessions", "--no-legend"])
                .output()
                .await;

            if let Ok(output) = output {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 3 {
                        let user = parts[2].to_string();
                        let old_user = state.active_user.clone();
                        if old_user.as_ref() != Some(&user) {
                            state.active_user = Some(user.clone());
                            if old_user.is_some() {
                                drop(state);
                                info!("active user changed: {:?} -> {}", old_user, user);
                                trigger_event_for_all(&registry_for_user, "OnUserSwitch", Some(user)).await;
                                state = state_for_user.lock().await;
                            }
                        }
                        break;
                    }
                }
            }

            drop(state);
        }
    });

    let registry_for_route = registry.clone();
    let state_for_route = system_state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let mut state = state_for_route.lock().await;
            let output = tokio::process::Command::new("ip")
                .args(["route", "show", "default"])
                .output()
                .await;

            if let Ok(output) = output {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let route = stdout.trim().to_string();
                let route_opt = if route.is_empty() { None } else { Some(route.clone()) };
                if state.default_route != route_opt {
                    let old_route = state.default_route.clone();
                    state.default_route = route_opt.clone();
                    if old_route.is_some() || route_opt.is_some() {
                        drop(state);
                        info!("default route changed: {:?} -> {:?}", old_route, route_opt);
                        trigger_event_for_all(&registry_for_route, "OnDefaultRouteChange", route_opt).await;
                        state = state_for_route.lock().await;
                    }
                }
            }

            drop(state);
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

async fn trigger_event_for_all(registry: &PluginRegistry, event_name: &str, event_data: Option<String>) {
    let locked_registry = registry.lock().await;
    for (_, plugin) in locked_registry.iter() {
        if plugin.enabled && plugin.event.contains(&event_name.to_string()) {
            info!("triggering {} for event {}", plugin.name, event_name);
            if let Err(e) = execute_plugin(plugin, event_name, None, event_data.clone()).await {
                error!("failed to execute plugin {} for {}: {}", plugin.name, event_name, e);
            }
        }
    }
}

async fn get_network_interfaces() -> HashMap<String, NetworkInterfaceState> {
    let mut interfaces = HashMap::new();
    let net_path = PathBuf::from("/sys/class/net");
    if let Ok(mut entries) = tokio::fs::read_dir(&net_path).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "lo" {
                continue;
            }

            let operstate_path = entry.path().join("operstate");
            let is_up = if let Ok(state) = tokio::fs::read_to_string(&operstate_path).await {
                state.trim() == "up"
            } else {
                false
            };

            let interface_type = if entry.path().join("wireless").exists() {
                "wifi".to_string()
            } else if name.starts_with("eth") || name.starts_with("en") {
                "ethernet".to_string()
            } else if name.starts_with("wl") {
                "wifi".to_string()
            } else if name.starts_with("docker") || name.starts_with("br-") {
                "bridge".to_string()
            } else if name.starts_with("veth") {
                "virtual".to_string()
            } else {
                "unknown".to_string()
            };

            let ssid = if interface_type == "wifi" && is_up {
                let output = tokio::process::Command::new("iwgetid")
                    .args(["-r", &name])
                    .output()
                    .await;

                output.ok().and_then(|o| {
                    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if s.is_empty() { None } else { Some(s) }
                })
            } else {
                None
            };

            let ip_addresses = get_interface_ips(&name).await;
            interfaces.insert(name.clone(), NetworkInterfaceState {
                name,
                is_up,
                ip_addresses,
                interface_type,
                ssid,
            });
        }
    }

    interfaces
}

async fn get_interface_ips(interface: &str) -> Vec<String> {
    let output = tokio::process::Command::new("ip")
        .args(["-o", "-4", "addr", "show", interface])
        .output()
        .await;

    let mut ips = Vec::new();
    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if let Some(inet_part) = line.split_whitespace().nth(3) {
                if let Some(ip) = inet_part.split('/').next() {
                    ips.push(ip.to_string());
                }
            }
        }
    }

    ips
}

async fn get_usb_devices() -> Vec<UsbDevice> {
    let mut devices = Vec::new();
    let output = tokio::process::Command::new("lsusb")
        .output()
        .await;

    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let parts: Vec<&str> = line.splitn(7, ' ').collect();
            if parts.len() >= 7 {
                if let Some(id_part) = parts.get(5) {
                    let id_parts: Vec<&str> = id_part.split(':').collect();
                    if id_parts.len() == 2 {
                        let vendor_id = id_parts[0].to_string();
                        let product_id = id_parts[1].to_string();
                        let name = parts.get(6).unwrap_or(&"Unknown").to_string();
                        let path = format!("{}:{}", parts.get(1).unwrap_or(&""), parts.get(3).unwrap_or(&"").trim_end_matches(':'));
                        devices.push(UsbDevice {
                            vendor_id,
                            product_id,
                            name,
                            path,
                        });
                    }
                }
            }
        }
    }

    devices
}

async fn get_connected_displays() -> Vec<String> {
    let mut displays = Vec::new();
    let drm_path = PathBuf::from("/sys/class/drm");
    if let Ok(mut entries) = tokio::fs::read_dir(&drm_path).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains("card") && name.contains("-") {
                let status_path = entry.path().join("status");
                if let Ok(status) = tokio::fs::read_to_string(&status_path).await {
                    if status.trim() == "connected" {
                        displays.push(name);
                    }
                }
            }
        }
    }

    displays
}

async fn get_disk_usage() -> HashMap<String, DiskUsage> {
    let mut usage = HashMap::new();
    let output = tokio::process::Command::new("df")
        .args(["-B1", "--output=target,size,used,avail,pcent"])
        .output()
        .await;

    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 5 {
                let mount_point = parts[0].to_string();
                if !mount_point.starts_with('/') || mount_point.starts_with("/snap") || mount_point.starts_with("/sys") || mount_point.starts_with("/proc") {
                    continue;
                }

                if let (Ok(total), Ok(used), Ok(available)) = (
                    parts[1].parse::<u64>(),
                    parts[2].parse::<u64>(),
                    parts[3].parse::<u64>()
                ) {
                    let percent_str = parts[4].trim_end_matches('%');
                    let usage_percent = percent_str.parse::<u8>().unwrap_or(0);
                    usage.insert(mount_point.clone(), DiskUsage {
                        mount_point,
                        total_bytes: total,
                        used_bytes: used,
                        available_bytes: available,
                        usage_percent,
                    });
                }
            }
        }
    }

    usage
}

async fn get_bluetooth_devices() -> Vec<BluetoothDevice> {
    let mut devices = Vec::new();
    let output = tokio::process::Command::new("bluetoothctl")
        .args(["devices", "Connected"])
        .output()
        .await;

    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.starts_with("Device ") {
                let parts: Vec<&str> = line.splitn(3, ' ').collect();
                if parts.len() >= 3 {
                    devices.push(BluetoothDevice {
                        address: parts[1].to_string(),
                        name: parts[2].to_string(),
                        connected: true,
                    });
                }
            }
        }
    }

    devices
}

async fn handle_client(mut stream: UnixStream, registry: PluginRegistry, cmd_tx: tokio::sync::mpsc::Sender<WatcherCommand>) -> Result<()> {
    let mut buffer = [0; 4096];
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

            for (_, plugin) in locked_registry.iter() {
                if plugin.enabled && plugin.event.contains(&"OnReload".to_string()) {
                    info!("triggering {} for event OnReload", plugin.name);
                    if let Err(e) = execute_plugin(plugin, "OnReload", None, None).await {
                        error!("failed to execute plugin for OnReload: {}", e);
                    }
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
                match parse_manifest(&name, &manifest_path).await {
                    Ok(plugin) => {
                        plugins.insert(name, plugin);
                    }
                    Err(e) => {
                        warn!("failed to parse manifest for {}: {}", name, e);
                    }
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
    let slot_cmd = "readarray -t CURRENT_SLOT < <(obsidianctl status | grep \"Slot\" | awk '{print $NF}' | tail -n +2) && echo $CURRENT_SLOT > /tmp/slot.tmp";
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
