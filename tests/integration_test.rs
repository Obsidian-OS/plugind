use std::process::{Command, Child};
use std::time::Duration;
use std::thread::sleep;
use std::path::PathBuf;
use tempfile::tempdir;
fn build_project() {
    let output = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .output()
        .expect("Failed to build project");
    if !output.status.success() {
        panic!("Project build failed: {:?}", output);
    }
}

fn start_daemon(socket_path: &PathBuf, log_dir: &PathBuf) -> Child {
    let cmd = Command::new("target/release/plugind")
        .env("SOCKET_PATH", socket_path.to_str().unwrap())
        .env("LOG_DIRECTORY", log_dir.to_str().unwrap())
        .spawn()
        .expect("Failed to start plugind daemon");
    sleep(Duration::from_secs(3)); 
    cmd
}

#[test]
fn test_pluginctl_list_command() {
    build_project();
    let temp_dir = tempdir().expect("Failed to create temporary directory");
    let socket_path = temp_dir.path().join("plugind.sock");
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir(&log_dir).expect("Failed to create temporary log directory");
    let mut daemon_process = start_daemon(&socket_path, &log_dir);
    let pluginctl_output = Command::new("target/release/pluginctl")
        .env("SOCKET_PATH", socket_path.to_str().unwrap())
        .arg("list")
        .output()
        .expect("Failed to execute pluginctl list command");

    assert!(pluginctl_output.status.success(), "pluginctl command failed: {:?}", pluginctl_output);
    let stdout = String::from_utf8_lossy(&pluginctl_output.stdout);
    println!("pluginctl list output:\n{}", stdout);
    assert!(stdout.contains("Name"), "Output does not contain 'Name' header");
    assert!(stdout.contains("Slot"), "Output does not contain 'Slot' header");
    assert!(stdout.contains("Event"), "Output does not contain 'Event' header");
    assert!(stdout.contains("Run As"), "Output does not contain 'Run As' header");
    assert!(stdout.contains("Enabled"), "Output does not contain 'Enabled' header");
    daemon_process.kill().expect("Failed to kill daemon process");
    daemon_process.wait().expect("Failed to wait for daemon process");
}
