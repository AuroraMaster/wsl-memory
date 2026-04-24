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
irm https://github.com/AuroraMaster/wsl-memory/releases/latest/download/install-host.ps1 | iex
```

Then run the guest installer inside each WSL distro:

```bash
curl -fsSL https://github.com/AuroraMaster/wsl-memory/releases/latest/download/install-guest.sh | sudo sh
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
listen_ip: "0.0.0.0"
listen_port: 15555
token_path: 'C:\Users\Public\wsl_agent_token'
remote_ips:
  - "127.0.0.1"
  - "::1"
  - "172.16.0.0/12"
logging:
  level: info
  max_file_size_mb: 8
  max_files: 5
  max_age_days: 7
reclamation:
  baseline_gap: 2147483648
  gap_ratio_mild: 0.01
  gap_ratio_moderate: 0.03
  gap_ratio_heavy: 0.06
  gap_ratio_critical: 0.12
  host_memory_warning: 0.70
  host_memory_pressure: 0.80
  guest_cpu_idle: 0.05
  guest_mem_available_idle_ratio: 0.15
  guest_io_idle: 10.0
  reclaim_ratio_moderate: 0.20
  reclaim_ratio_heavy: 0.50
  reclaim_floor_ratio: 0.001
  reclaim_cap_moderate: 0.008
  reclaim_cap_heavy: 0.016
  sustained_windows: 3
  cooldown_moderate:
    secs: 10
    nanos: 0
  cooldown_heavy:
    secs: 30
    nanos: 0
  cooldown_critical:
    secs: 600
    nanos: 0
```

If the file does not exist, the host creates it and writes the first available
recommended port. You can set any listen IP and port in this file. Legacy
`listen_addr: "0.0.0.0:15555"` is still accepted.

WSL guest config:

```yaml
# /usr/local/etc/wsl-memory-agent/config.yaml
host: auto:multi
token_path: /mnt/c/Users/Public/wsl_agent_token
interval: 4
allow_drop: false
multi_path: true
tcp: false
local_reclaim:
  cache_ratio_moderate: 0.50
  cache_ratio_heavy: 0.70
  avail_ratio_low: 0.15
  cpu_idle: 5.0
  io_idle: 10.0
  reclaim_fraction_moderate: 0.10
  reclaim_fraction_heavy: 0.25
  reclaim_floor_ratio: 0.002
  reclaim_cap_ratio: 0.010
  sustained_ticks: 3
  cooldown:
    secs: 15
    nanos: 0
  host_defer:
    secs: 30
    nanos: 0
  check_interval:
    secs: 5
    nanos: 0
```

`host` can be `auto:multi` for gateway discovery plus recommended-port probing,
or a fixed endpoint such as `172.24.128.1:15555`.

Both agents reload runtime-safe config every 5 seconds:

- Windows host: `token_path` and token file content
- WSL guest: `token_path`, `interval`, and `allow_drop`

Network-shaping fields (`listen_ip`, `listen_port`, `listen_addr`, `host`,
`multi_path`, `tcp`) require a service restart or reconnect because they affect
bound sockets and target selection.

The host-side `reclamation` section tunes the adaptive scoring thresholds and
the Critical `drop_caches` idle gate. `guest_cpu_idle` uses the Linux guest CPU
reported from `/proc/stat`; `guest_mem_available_idle_ratio` uses
`MemAvailable / MemTotal` from the guest `/proc/meminfo`. `guest_io_idle` is
kept for compatibility with older config files but no longer gates Critical
`drop_caches`.

The guest-side `local_reclaim` section tunes the offline fallback reclaimer.
These sections are read on startup, so restart the corresponding service after
editing them.

Host service logs now rotate by size and age using the `logging` section.
Guest logs still go to `journald`, with service-level rate limiting enabled in
the installed unit.

`allow_drop` is disabled by default. The guest prefers cgroup v2
`memory.reclaim`; `drop_caches` is only used when explicitly enabled, Critical
pressure is reached, and the guest is quiet by Linux CPU plus Linux memory
headroom.

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
schtasks /Query /TN WSLMemoryHost /V /FO LIST
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
