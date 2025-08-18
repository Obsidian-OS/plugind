# ObsidianOS Plugind

`plugind` is a daemon for ObsidianOS that manages and triggers system and user-defined plugins based on various events, including file system changes and system status updates. It provides a robust and flexible way to extend system functionality through simple, scriptable plugins.

## Features

- **Event-Driven Plugin Execution**: Triggers plugins based on file system events (e.g., file modifications, creations) and specific system events like battery level changes.
- **System and User Plugins**: Supports loading plugins from both system-wide (`/etc/plugins`) and user-specific (`~/.config/plugins`) directories.
- **Flexible Plugin Definition**: Plugins are defined by `manifest.plug` files, allowing for easy configuration of plugin name, associated events, execution slot, and user context.
- **User Impersonation**: Plugins can be configured to run as specific users or the calling user, enhancing security and flexibility.
- **CLI Control**: A companion `pluginctl` command-line tool allows for easy interaction with the daemon to list, enable, disable, trigger, and reload plugins.
- **Logging**: Comprehensive logging to `/var/log/plugind/daemon.log` for monitoring and debugging.

## Architecture

`plugind` operates as a background service, listening for commands on a Unix socket (`/tmp/plugind.sock`). It maintains a registry of available plugins and their states. When a configured event occurs, the daemon executes the corresponding plugin.

## Plugin Structure

Each plugin resides in its own directory and must contain a `manifest.plug` file. This file is sourced by `bash` to define plugin metadata. An example `manifest.plug`:

```bash
# manifest.plug example
SLOT="AB" # Optional: 'A', 'B', or 'AB'. Plugin only runs if current system slot matches.
EVENT="OnEnabled OnBatteryLevelChange" # Space-separated list of events that trigger this plugin
RUN_AS="@" # Optional: User to run the plugin as. '@' for calling user, '#' for root, or a specific username.
RESTART_ON="" # Not yet implemented
WATCH_PATH="/sys/class/power_supply/BAT0/capacity" # Optional: Path to watch for file system events

main() {
    # Your plugin logic here
    echo "Hello from plugin $EVENT_NAME!"
    if [ -n "$EVENT_RETURN" ]; then
        echo "Event data: $EVENT_RETURN"
    fi
}
```

- `SLOT`: (Optional) Specifies which system slot (A or B) the plugin is active for. `AB` means active for both. If the current slot doesn't match, the plugin is skipped.
- `EVENT`: A space-separated list of events that will trigger the `main` function within the plugin. Common events include `OnEnabled`, `OnDisabled`, `OnBatteryLevelChange`, or file system event types (e.g., `Modify`, `Create`).
- `RUN_AS`: (Optional) Defines the user under which the plugin's `main` function will be executed. `@` means the user who invoked the `ctl` command, `#` means `root`, and any other string is a specific username. If empty, it runs as the daemon user.
- `RESTART_ON`: (Currently not implemented).
- `WATCH_PATH`: (Optional) A file system path that, if modified, will trigger the plugin with the corresponding file system event type.

The `main` function within `manifest.plug` is the entry point for your plugin logic. The `EVENT_NAME` environment variable will contain the name of the event that triggered the plugin. For some events (like `OnBatteryLevelChange`), `EVENT_RETURN` will contain additional data.

## Usage

### Daemon

The `plugind` daemon typically runs in the background. It can be started as a system service.

### CLI (`pluginctl`)

The `pluginctl` command-line tool is used to interact with the `plugind` daemon.

```bash
# List all registered plugins
pluginctl list

# Get detailed information about a specific plugin
pluginctl info <plugin_name>

# Enable a plugin
pluginctl enable <plugin_name>

# Disable a plugin
pluginctl disable <plugin_name>

# Manually trigger a plugin with a specific event and optional data
pluginctl trigger <plugin_name> <event_name> [event_data]

# Reload all plugins (re-reads manifest files)
pluginctl reload

# Check daemon status (basic)
pluginctl status

# Get raw JSON output (for scripting)
pluginctl --raw list
```

## Installation

To install `plugind` and `pluginctl` from source, you will need to have Rust and Cargo installed. If you don't have them, you can install them using `rustup`:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Once Rust and Cargo are installed, navigate to the project's root directory and build the binaries:

```bash
cargo build --release
```

This will compile the `plugind` daemon and the `pluginctl` CLI tool. The compiled binaries will be located in `target/release/`.

You can then manually copy them to your system's PATH, for example:

```bash
sudo cp target/release/plugind /usr/local/bin/
sudo cp target/release/pluginctl /usr/local/bin/
```

For `plugind` to run as a system service, you would typically set up a systemd service unit. An example service file might look like this (you would need to create and enable it):

```
[Unit]
Description=ObsidianOS Plugin Daemon
After=network.target

[Service]
ExecStart=/usr/local/bin/plugind
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Save this as `/etc/systemd/system/plugind.service` and then:

```bash
sudo systemctl enable plugind
sudo systemctl start plugind
```

## Development

To contribute to `plugind` or `pluginctl`, or to build from source for development purposes:

1.  **Clone the repository**:
    ```bash
    git clone https://github.com/Obsidian-OS/plugind.git
    cd plugind
    ```

2.  **Build the project**:
    ```bash
    cargo build
    ```

3.  **Run tests**:
    ```bash
    cargo test
    ```

4.  **Run the daemon (for testing)**:
    ```bash
    cargo run --bin plugind
    ```

5.  **Run the CLI (for testing)**:
    ```bash
    cargo run --bin pluginctl -- <command>
    # Example:
    # cargo run --bin pluginctl -- list
    ```


