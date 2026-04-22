# WSL Memory Agent

[中文说明](README.zh-CN.md)

WSL Memory Agent is a small dual-agent tool for reducing WSL2 `vmmem` memory
bloat. It runs one agent on Windows and one agent inside WSL. The WSL side
reports memory pressure; the Windows side decides when reclaim is useful.

The project deliberately stays small: no GUI, no dashboard, and no long-running
database. Configuration is YAML.

## Quick Install

Run the Windows host installer first from an elevated PowerShell window:

```powershell
irm https://github.com/wsl-memory-agent/wsl-memory-agent/releases/latest/download/install-host.ps1 | iex
```

Then run the guest installer inside each WSL distro:

```bash
curl -fsSL https://github.com/wsl-memory-agent/wsl-memory-agent/releases/latest/download/install-guest.sh | sudo sh
```

The host installer creates a shared token at:

```text
C:\Users\Public\wsl_agent_token
```

The WSL installer reads it through:

```text
/mnt/c/Users/Public/wsl_agent_token
```

## Configuration

Windows host config:

```yaml
# %APPDATA%\WSLMemoryAgent\config.yaml
listen_addr: "0.0.0.0:15555"
token_path: 'C:\Users\Public\wsl_agent_token'
```

WSL guest config:

```yaml
# /usr/local/etc/wsl-memory-agent/config.yaml
host: auto:multi
token_path: /mnt/c/Users/Public/wsl_agent_token
interval: 4
allow_drop: false
multi_path: true
tcp: false
```

`allow_drop` is disabled by default. The guest prefers cgroup v2
`memory.reclaim`; `drop_caches` is only used when explicitly enabled and when
the agent sees critical idle pressure.

## Build

Requirements:

- Rust stable
- Zig and `cargo-zigbuild` for cross-target release builds

```bash
cargo test
cargo build --release --bin host
cargo build --release --bin guest
```

WSL/Linux release build:

```bash
cargo zigbuild --release --bin guest
```

Release assets expected by the installers:

```text
host.exe
wsl-memory-guest
install-host.ps1
install-guest.sh
```

## Commands

Windows host:

```powershell
.\host.exe
.\host.exe --install
.\host.exe --uninstall
.\host.exe --check-port
.\host.exe --listen 0.0.0.0:15555
```

WSL guest:

```bash
sudo wsl-memory-guest --install
sudo wsl-memory-guest --status
sudo systemctl restart wsl-memory-guest
sudo journalctl -u wsl-memory-guest -f
sudo wsl-memory-guest --uninstall
```

## How It Works

WSL2 can keep large Linux page cache inside the Windows `vmmem` process after
the workload has already stopped using that memory. The guest agent reports
resident memory, cache, CPU, and IO. The host agent compares that with Windows
host pressure and `vmmem` RSS, then sends conservative reclaim requests only
when the score is high enough.

The main reclaim path is cgroup v2 `memory.reclaim`, which asks the Linux kernel
to reclaim memory from the guest without killing processes.

## Repository Layout

```text
src/                  Rust source
scripts/              one-line install scripts
examples/             YAML config examples
debian/               systemd unit and deb package scripts
.github/workflows/    CI checks
```

## Troubleshooting

Check the Windows service:

```powershell
Get-Service WSLMemoryHost
```

Check the WSL service:

```bash
sudo systemctl status wsl-memory-guest --no-pager
sudo journalctl -u wsl-memory-guest -n 100 --no-pager
```

Check WSL gateway discovery:

```bash
ip route
cat /etc/resolv.conf
```

## References

- [microsoft/WSL issue #4166](https://github.com/microsoft/WSL/issues/4166)
- [cgroup v2 memory.reclaim](https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html#memory)
- [Linux drop_caches](https://www.kernel.org/doc/Documentation/sysctl/vm.txt)

## License

MIT
