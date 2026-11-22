# Startup Modes

The `server` binary supports several startup modes via `--startup-mode` (or `-s`). The configuration file path can be set with `-c`.

```bash
cargo run -- --startup-mode <MODE>
cargo run -- --help
```

## Available Modes

| Mode | Role |
|---|---|
| `default` | Full mode — runs Signaling + Desk Server + WebRTC + Capture in a single process. |
| `signaling` | Signaling service only (Signaling + TURN). |
| `desk-server` | Desk server only (controlled device). |
| `service-daemon` | System service daemon (SYSTEM / root) that manages per-session workers. |
| `session-worker` | Worker process launched by the daemon inside the user's desktop session. |
| `mcp-stdio` | Read-only MCP server over stdio for local AI agents. |

## Default Mode

The simplest deployment: the same logical daemon → peer connection → worker pipeline runs inside one OS process and uses in-process channels. It is ideal for portable use and development.

## Service-Daemon Process Model

To capture secure environments like the Windows **UAC** or **lock screen**, the service-daemon mode splits operations across privilege boundaries:

![Service-daemon process and IPC model](/architecture/process-model.svg)

The **ServiceDaemon** (running as SYSTEM / root) owns the WebRTC connection, signaling, and child processes. It spawns a **SessionWorker** inside each desktop session for capture, encoding, input, files, and clipboard.

They use three independent transports: a bidirectional **event pipe** for signaling and control, a one-way **media pipe** for encoded audio/video frames, and a bidirectional **file pipe** for file commands and chunks. Keeping file transfer separate prevents its backpressure from blocking control events.

This split lets session workers restart during user switching **without dropping the browser connection** — the peer connection lives in the daemon.

Windows uses the native Service Control Manager. Linux provides a systemd service installer; macOS does not use this system-service installation path.

### Install, update and remove the Windows service

Run these from an elevated prompt with the server binary of the build you want to install (`static/` must sit next to it):

- `--install-service [--install-path <dir>] [--config-file-path <config.toml>] [--install-idd-driver]` copies the build (default `C:\Program Files\LCXBOX Agent Desk`), registers `LcxboxDeskService` and starts it. It refuses when the service is already installed.
- `--update-service` replaces the installed build in place: the new files are staged next to the old ones while the service still runs, then the service is stopped, the files are swapped and the service is started again. The service keeps its directory, start type and `--config-file-path` unless you pass a new one; other hand-added launch arguments are not kept. If the new build does not start, the previous one is restored and started.
- `--uninstall-service` stops the service, removes the virtual display driver, the service and the install directory. Data and configuration are kept.

When you stop the service yourself, `Stop-Service` returns a few milliseconds before the service process exits; wait for the process before replacing files by hand, or use `--update-service`.

### Install the Linux systemd service

In the local desktop client, sign in as the device owner and open **System Settings → Linux System Service**. Choose **Install Service**, then approve the desktop's administrator authorization prompt. Installation requires systemd, `pkexec`, and a working desktop authentication agent. The client never receives your administrator password. No experimental opt-in is required.

The installation path is fixed at `/usr/lib/lcxbox-agent-desk`; the system configuration is stored in `/etc/lcxbox-agent-desk/config.toml`. The daemon runs as root, while desktop workers run as the logged-in user. Windows virtual display driver options do not apply to Linux.

The dialog waits for the installer process to finish and reports completion, cancelled or failed authorization, missing `pkexec`, an operation already in progress, or an installer failure. A submitted request alone is not an installation success. The local client can report completion even if uninstalling stops its connection to the daemon. If no final result is available, it reports an unknown result without automatically repeating the operation. The service card separately shows whether the service is installed and running.

**Uninstall Service** stops and disables the service and removes its systemd unit. Installed program files and configuration are retained. Administrator authorization is required for both installation and removal. For a command-line installation, run the server with administrator privileges and `--install-service --config-file-path /absolute/path/to/config.toml` (the first installation needs the current user configuration); use `--uninstall-service` for removal.

Service installation does not grant desktop capture or input permissions. Linux AI desktop support targets a logged-in GNOME Wayland session; a successful installation does not establish login-screen access or complete unattended desktop support. Desktop authorization and real-machine installation/uninstallation need separate validation.

## MCP stdio Mode

`--startup-mode mcp-stdio` turns the device into a [read-only MCP server](/features/mcp-server). In this mode stdin/stdout carry MCP JSON-RPC, so the server must never log to stdout.
