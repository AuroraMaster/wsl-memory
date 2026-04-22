use clap::{ArgAction, Parser};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::sleep;
use tracing::{error, info, warn};
use wsl_memory_agent::{
    diagnose_port_conflicts, GuestLocalAction, GuestLocalConfig, GuestLocalMetrics,
    GuestLocalReclaimer, MultiPathConfig, MultiPathConnector, RECOMMENDED_PORTS,
};

#[cfg(unix)]
const SERVICE_NAME: &str = "wsl-memory-guest";
#[cfg(unix)]
const INSTALL_PATH: &str = "/usr/local/bin/wsl-memory-guest";
#[cfg(unix)]
const SERVICE_FILE: &str = "/etc/systemd/system/wsl-memory-guest.service";
const CONFIG_FILE: &str = "/usr/local/etc/wsl-memory-agent/config.yaml";

#[derive(Parser, Debug, Clone)]
#[command(about = "WSL Memory Guest Agent — collect metrics and execute reclamation")]
struct Opt {
    /// YAML config file in the WSL install prefix.
    #[arg(long, default_value = CONFIG_FILE)]
    config: PathBuf,

    #[arg(long)]
    host: Option<String>,

    #[arg(long)]
    token_path: Option<PathBuf>,

    #[arg(long)]
    interval: Option<u64>,

    #[arg(long)]
    allow_drop: bool,

    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_multi_path")]
    multi_path: bool,

    #[arg(long, action = ArgAction::SetTrue)]
    no_multi_path: bool,

    /// Install as a systemd service and start
    #[arg(long)]
    install: bool,

    /// Uninstall the systemd service
    #[arg(long)]
    uninstall: bool,

    /// Show service status
    #[arg(long)]
    status: bool,

    /// Run port diagnostics
    #[arg(long)]
    check_port: bool,

    /// Use TCP instead of UDP (fallback for unusual network configs)
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "udp")]
    tcp: bool,

    /// Use UDP instead of TCP when the config enables TCP.
    #[arg(long, action = ArgAction::SetTrue)]
    udp: bool,
}

#[derive(Clone)]
struct GuestRuntimeConfig {
    token: String,
    interval: u64,
    allow_drop: bool,
}

const CONFIG_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct GuestConfig {
    host: String,
    token_path: PathBuf,
    interval: u64,
    allow_drop: bool,
    multi_path: bool,
    tcp: bool,
}

impl Default for GuestConfig {
    fn default() -> Self {
        Self {
            host: "auto:multi".to_string(),
            token_path: PathBuf::from("/mnt/c/Users/Public/wsl_agent_token"),
            interval: 4,
            allow_drop: false,
            multi_path: true,
            tcp: false,
        }
    }
}

impl GuestConfig {
    fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let content = fs::read_to_string(path)?;
            Ok(serde_yml::from_str(&content)?)
        } else {
            Ok(Self::default())
        }
    }

    fn merged_with_cli(mut self, opt: &Opt) -> Self {
        if let Some(host) = &opt.host {
            self.host.clone_from(host);
        }
        if let Some(token_path) = &opt.token_path {
            self.token_path.clone_from(token_path);
        }
        if let Some(interval) = opt.interval {
            self.interval = interval;
        }
        if opt.allow_drop {
            self.allow_drop = true;
        }
        if opt.multi_path {
            self.multi_path = true;
        }
        if opt.no_multi_path {
            self.multi_path = false;
        }
        if opt.tcp {
            self.tcp = true;
        }
        if opt.udp {
            self.tcp = false;
        }
        self
    }
}

fn load_runtime_config(config_path: &Path, opt: &Opt) -> anyhow::Result<GuestRuntimeConfig> {
    let cfg = GuestConfig::load(config_path)?.merged_with_cli(opt);
    let token = fs::read_to_string(&cfg.token_path)
        .map(|s| s.trim().to_string())
        .map_err(|e| anyhow::anyhow!("failed read token at {:?}: {}", cfg.token_path, e))?;
    if token.is_empty() {
        anyhow::bail!("token at {:?} is empty", cfg.token_path);
    }
    Ok(GuestRuntimeConfig {
        token,
        interval: cfg.interval.max(1),
        allow_drop: cfg.allow_drop,
    })
}

fn current_runtime(runtime: &Arc<RwLock<GuestRuntimeConfig>>) -> GuestRuntimeConfig {
    runtime
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| GuestRuntimeConfig {
            token: String::new(),
            interval: 4,
            allow_drop: false,
        })
}

fn spawn_config_reloader(
    config_path: PathBuf,
    opt: Opt,
    initial: GuestRuntimeConfig,
) -> Arc<RwLock<GuestRuntimeConfig>> {
    let runtime = Arc::new(RwLock::new(initial));
    let shared = Arc::clone(&runtime);
    tokio::spawn(async move {
        loop {
            sleep(CONFIG_REFRESH_INTERVAL).await;
            match load_runtime_config(&config_path, &opt) {
                Ok(next) => {
                    if let Ok(mut guard) = shared.write() {
                        *guard = next;
                    }
                }
                Err(e) => warn!("failed reload config: {}", e),
            }
        }
    });
    runtime
}

#[cfg(unix)]
fn write_default_config_if_missing(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_yml::to_string(&GuestConfig::default())?;
    fs::write(path, content)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Service management (Linux systemd)
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn check_root() {
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        eprintln!("Error: root privileges required. Run with sudo.");
        std::process::exit(1);
    }
}

#[cfg(unix)]
fn install_service(config_path: &Path) -> anyhow::Result<()> {
    check_root();

    let exe = std::env::current_exe()?;
    println!("Copying binary to {} ...", INSTALL_PATH);
    fs::copy(&exe, INSTALL_PATH)?;

    fs::set_permissions(INSTALL_PATH, fs::Permissions::from_mode(0o755))?;

    println!("Writing default config to {} ...", config_path.display());
    write_default_config_if_missing(config_path)?;

    println!("Writing systemd unit ...");
    fs::write(SERVICE_FILE, systemd_unit(config_path))?;

    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", "--now", SERVICE_NAME])?;

    println!("\n=== Installed ===");
    println!("Service: {}", SERVICE_NAME);
    println!("Binary:  {}", INSTALL_PATH);
    println!("Config:  {}", config_path.display());
    println!("Unit:    {}", SERVICE_FILE);
    println!("\nManagement commands:");
    println!("  sudo systemctl status  {}", SERVICE_NAME);
    println!("  sudo systemctl restart {}", SERVICE_NAME);
    println!("  sudo journalctl -u {} -f", SERVICE_NAME);
    println!("  sudo {} --uninstall", INSTALL_PATH);
    Ok(())
}

#[cfg(not(unix))]
fn install_service(_config_path: &Path) -> anyhow::Result<()> {
    anyhow::bail!("guest service installation is only supported on Linux/WSL");
}

#[cfg(unix)]
fn uninstall_service() -> anyhow::Result<()> {
    check_root();

    if is_active() {
        println!("Stopping service ...");
        let _ = run_systemctl(&["stop", SERVICE_NAME]);
    }
    if is_enabled() {
        println!("Disabling service ...");
        let _ = run_systemctl(&["disable", SERVICE_NAME]);
    }
    if PathBuf::from(SERVICE_FILE).exists() {
        println!("Removing unit file ...");
        fs::remove_file(SERVICE_FILE)?;
    }
    if PathBuf::from(INSTALL_PATH).exists() {
        println!("Removing binary ...");
        fs::remove_file(INSTALL_PATH)?;
    }
    if PathBuf::from(CONFIG_FILE).exists() {
        println!(
            "Keeping config at {} (remove manually if not needed).",
            CONFIG_FILE
        );
    }
    let _ = run_systemctl(&["daemon-reload"]);

    println!("Service uninstalled.");
    Ok(())
}

#[cfg(not(unix))]
fn uninstall_service() -> anyhow::Result<()> {
    anyhow::bail!("guest service uninstall is only supported on Linux/WSL");
}

#[cfg(unix)]
fn show_status() -> anyhow::Result<()> {
    if !PathBuf::from(SERVICE_FILE).exists() {
        println!("Service is not installed.");
        println!(
            "Install with: sudo {} --install",
            std::env::current_exe()?.display()
        );
        return Ok(());
    }
    let _ = std::process::Command::new("systemctl")
        .args(["status", SERVICE_NAME, "--no-pager"])
        .status();
    println!();
    println!("Recent logs:");
    let _ = std::process::Command::new("journalctl")
        .args(["-u", SERVICE_NAME, "-n", "20", "--no-pager"])
        .status();
    Ok(())
}

#[cfg(not(unix))]
fn show_status() -> anyhow::Result<()> {
    anyhow::bail!("guest service status is only supported on Linux/WSL");
}

#[cfg(unix)]
fn run_systemctl(args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(args)
        .status()?;
    if !status.success() {
        anyhow::bail!("systemctl {} failed with {}", args.join(" "), status);
    }
    Ok(())
}

#[cfg(unix)]
fn is_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", SERVICE_NAME])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn is_enabled() -> bool {
    std::process::Command::new("systemctl")
        .args(["is-enabled", "--quiet", SERVICE_NAME])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn systemd_unit(config_path: &Path) -> String {
    format!(
        r#"[Unit]
Description=WSL Memory Guest Agent
Documentation=https://github.com/microsoft/WSL/issues/4166
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={} --config {}
Restart=always
RestartSec=5
User=root
StandardOutput=journal
StandardError=journal
SyslogIdentifier={}
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/sys/fs/cgroup /proc/sys/vm/compact_memory /proc/sys/vm/drop_caches

[Install]
WantedBy=multi-user.target
"#,
        INSTALL_PATH,
        config_path.display(),
        SERVICE_NAME
    )
}

// ---------------------------------------------------------------------------
// Protocol types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct MetricsMsg<'a> {
    msg_type: &'a str,
    token: &'a str,
    distro: &'a str,
    resident: u64,
    file_cache: u64,
    anon: u64,
    cpu_percent: f32,
    io_rate: f32,
}

#[derive(Deserialize)]
struct CommandMsg {
    #[allow(dead_code)]
    msg_type: String,
    cmd_id: String,
    action: String,
    bytes: Option<u64>,
    level: Option<u8>,
    #[allow(dead_code)]
    reason: Option<String>,
    pressure_level: Option<String>,
}

#[derive(Serialize)]
struct ResultMsg<'a> {
    msg_type: &'a str,
    cmd_id: &'a str,
    status: &'a str,
    freed_bytes: Option<u64>,
    note: &'a str,
}

// ---------------------------------------------------------------------------
// Metrics collection
// ---------------------------------------------------------------------------

fn parse_meminfo() -> std::io::Result<(u64, u64, u64)> {
    let s = fs::read_to_string("/proc/meminfo")?;
    let mut total = 0u64;
    let mut cached = 0u64;
    let mut s_reclaim = 0u64;
    let mut buffers = 0u64;
    let mut free = 0u64;

    for line in s.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let val = parts[1].parse::<u64>().unwrap_or(0) * 1024;
        match parts[0] {
            "MemTotal:" => total = val,
            "Cached:" => cached = val,
            "SReclaimable:" => s_reclaim = val,
            "Buffers:" => buffers = val,
            "MemFree:" => free = val,
            _ => {}
        }
    }

    let file_cache = cached + s_reclaim + buffers;
    let resident = total.saturating_sub(free);
    let anon = resident.saturating_sub(file_cache);
    Ok((resident, file_cache, anon))
}

/// Cross-tick CPU sampler — no blocking sleep.  Uses the natural tick
/// interval (4-5 s) as the sampling window instead of wasting 100 ms.
struct CpuSampler {
    prev_total: u64,
    prev_active: u64,
}

impl CpuSampler {
    fn new() -> Self {
        let (t, a) = Self::read_proc_stat();
        Self {
            prev_total: t,
            prev_active: a,
        }
    }

    fn sample(&mut self) -> f32 {
        let (total, active) = Self::read_proc_stat();
        let dt = total.saturating_sub(self.prev_total);
        let da = active.saturating_sub(self.prev_active);
        self.prev_total = total;
        self.prev_active = active;
        if dt > 0 {
            da as f32 / dt as f32 * 100.0
        } else {
            0.0
        }
    }

    fn read_proc_stat() -> (u64, u64) {
        let s = match fs::read_to_string("/proc/stat") {
            Ok(s) => s,
            Err(_) => return (0, 0),
        };
        let line = match s.lines().next() {
            Some(l) if l.starts_with("cpu ") => l,
            _ => return (0, 0),
        };
        let parts: Vec<&str> = line.split_whitespace().collect();
        let get = |i: usize| -> u64 { parts.get(i).and_then(|s| s.parse().ok()).unwrap_or(0) };
        let (user, nice, system, idle, iowait) = (get(1), get(2), get(3), get(4), get(5));
        (user + nice + system + idle + iowait, user + nice + system)
    }
}

fn read_disk_sectors() -> u64 {
    let s = match fs::read_to_string("/proc/diskstats") {
        Ok(s) => s,
        Err(_) => return 0,
    };
    s.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() > 9 {
                let read: u64 = parts[5].parse().ok()?;
                let write: u64 = parts[9].parse().ok()?;
                Some(read + write)
            } else {
                None
            }
        })
        .sum()
}

struct IoSampler {
    prev_sectors: u64,
    prev_at: Instant,
}

impl IoSampler {
    fn new() -> Self {
        Self {
            prev_sectors: read_disk_sectors(),
            prev_at: Instant::now(),
        }
    }

    fn sample(&mut self) -> f32 {
        let now = Instant::now();
        let sectors = read_disk_sectors();
        let delta = sectors.saturating_sub(self.prev_sectors);
        let elapsed = now.duration_since(self.prev_at).as_secs_f32();
        self.prev_sectors = sectors;
        self.prev_at = now;
        if elapsed > 0.0 {
            delta as f32 * 512.0 / 1024.0 / 1024.0 / elapsed
        } else {
            0.0
        }
    }
}

fn discover_gateway_ip() -> Option<String> {
    let out = std::process::Command::new("ip")
        .arg("route")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        if line.starts_with("default") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if let Some(idx) = parts.iter().position(|p| *p == "via") {
                return parts.get(idx + 1).map(|s| s.to_string());
            }
        }
    }

    if let Ok(s) = fs::read_to_string("/etc/resolv.conf") {
        for line in s.lines() {
            if line.starts_with("nameserver") {
                if let Some(ip) = line.split_whitespace().nth(1) {
                    return Some(ip.to_string());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Kernel interface probing — detect what reclamation knobs exist at runtime.
// ---------------------------------------------------------------------------

/// Available reclamation interfaces, probed once at startup.
#[derive(Debug)]
struct ReclaimCapabilities {
    /// Path to `memory.reclaim` (cgroup v2).
    /// Requires kernel >= 5.19, cgroup v2, and CONFIG_MEMCG=y.
    memory_reclaim: Option<String>,
    /// `/proc/sys/vm/compact_memory` — requires CONFIG_COMPACTION=y.
    compact: bool,
    /// `/proc/sys/vm/drop_caches` — available since Linux 2.6.16.
    drop_caches: bool,
}

static CAPS: OnceLock<ReclaimCapabilities> = OnceLock::new();

fn probe_capabilities() -> ReclaimCapabilities {
    let memory_reclaim = detect_memory_reclaim_path();
    let compact = Path::new("/proc/sys/vm/compact_memory").exists();
    let drop_caches = Path::new("/proc/sys/vm/drop_caches").exists();

    info!(
        "reclaim capabilities: memory.reclaim={}, compact={}, drop_caches={}",
        memory_reclaim.as_deref().unwrap_or("unavailable"),
        compact,
        drop_caches,
    );

    ReclaimCapabilities {
        memory_reclaim,
        compact,
        drop_caches,
    }
}

/// Resolve the `memory.reclaim` path for the current process's cgroup.
///
/// WSL2 typically runs everything in the root cgroup (`/`), but Docker or
/// systemd scopes place processes in sub-cgroups.  We read
/// `/proc/self/cgroup` (cgroup v2: single `0::/<path>` line) and construct
/// the full sysfs path.  Falls back to `/sys/fs/cgroup/memory.reclaim` if
/// detection fails.
fn detect_memory_reclaim_path() -> Option<String> {
    let cgroup_base = "/sys/fs/cgroup";

    let rel = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.starts_with("0::"))
                .map(|l| l.trim_start_matches("0::").trim().to_string())
        })
        .unwrap_or_else(|| "/".to_string());

    let rel_clean = rel.trim_matches('/');
    let path = if rel_clean.is_empty() {
        format!("{}/memory.reclaim", cgroup_base)
    } else {
        format!("{}/{}/memory.reclaim", cgroup_base, rel_clean)
    };

    if Path::new(&path).exists() {
        return Some(path);
    }

    let fallback = format!("{}/memory.reclaim", cgroup_base);
    if Path::new(&fallback).exists() {
        return Some(fallback);
    }

    None
}

fn caps() -> &'static ReclaimCapabilities {
    CAPS.get_or_init(probe_capabilities)
}

// ---------------------------------------------------------------------------
// Command execution — with runtime capability checks and fallback chain.
//
// Reclaim: memory.reclaim → (fallback) compact → report error
// Compact: compact_memory → report error
// DropCaches: sync + drop_caches → report error
// ---------------------------------------------------------------------------

async fn execute_reclaim(bytes: u64) -> Result<u64, String> {
    info!("executing reclaim: {} bytes", bytes);
    let before_cache = parse_meminfo().ok().map(|(_, c, _)| c).unwrap_or(0);

    if let Some(path) = &caps().memory_reclaim {
        match fs::write(path, bytes.to_string()) {
            Ok(()) => {
                sleep(Duration::from_secs(2)).await;
                let after = parse_meminfo().ok().map(|(_, c, _)| c).unwrap_or(0);
                let freed = before_cache.saturating_sub(after);
                info!("reclaim via {} freed ~{} bytes", path, freed);
                return Ok(freed);
            }
            Err(e) => {
                warn!("memory.reclaim write failed ({}): {}", path, e);
            }
        }
    } else {
        warn!("memory.reclaim unavailable (needs cgroup v2 + kernel >= 5.19)");
    }

    if caps().compact {
        info!("falling back to compact_memory");
        let _ = fs::write("/proc/sys/vm/compact_memory", "1");
        sleep(Duration::from_secs(2)).await;
        let after = parse_meminfo().ok().map(|(_, c, _)| c).unwrap_or(0);
        let freed = before_cache.saturating_sub(after);
        return Ok(freed);
    }

    Err("no reclamation interface available: \
         memory.reclaim requires cgroup v2 + kernel >= 5.19; \
         compact_memory requires CONFIG_COMPACTION=y"
        .to_string())
}

async fn execute_compact() -> Result<(), String> {
    if !caps().compact {
        return Err("compact_memory unavailable (CONFIG_COMPACTION not enabled)".to_string());
    }
    info!("executing memory compaction");
    fs::write("/proc/sys/vm/compact_memory", "1")
        .map_err(|e| format!("compact write failed: {}", e))?;
    sleep(Duration::from_secs(2)).await;
    Ok(())
}

async fn execute_drop_caches(level: u8, allow_drop: bool) -> Result<(), String> {
    if !allow_drop {
        return Err("drop_caches not allowed (use --allow-drop)".to_string());
    }
    if !caps().drop_caches {
        return Err("drop_caches unavailable on this kernel".to_string());
    }
    warn!("executing drop_caches level {} (DISRUPTIVE)", level);
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("sync && echo {} > /proc/sys/vm/drop_caches", level))
        .status()
        .map_err(|e| format!("drop_caches failed: {}", e))?;
    Ok(())
}

async fn handle_command(cmd: &CommandMsg, allow_drop: bool) -> Option<Vec<u8>> {
    if let Some(pressure) = &cmd.pressure_level {
        info!(
            "command: {} action={} pressure={}",
            cmd.cmd_id, cmd.action, pressure
        );
    } else {
        info!("command: {} action={}", cmd.cmd_id, cmd.action);
    }

    let (status, freed_bytes, note) = match cmd.action.as_str() {
        "reclaim" => match cmd.bytes {
            Some(bytes) => match execute_reclaim(bytes).await {
                Ok(freed) => ("ok", Some(freed), "reclaim executed".to_string()),
                Err(e) => ("error", None, e),
            },
            None => return None,
        },
        "compact" => match execute_compact().await {
            Ok(()) => ("ok", None, "compact executed".to_string()),
            Err(e) => ("error", None, e),
        },
        "drop_caches" => {
            let level = cmd.level.unwrap_or(3);
            match execute_drop_caches(level, allow_drop).await {
                Ok(()) => ("ok", None, "drop_caches executed".to_string()),
                Err(e) => ("error", None, e),
            }
        }
        _ => return None,
    };

    let res = ResultMsg {
        msg_type: "result",
        cmd_id: &cmd.cmd_id,
        status,
        freed_bytes,
        note: &note,
    };
    serde_json::to_vec(&res).ok()
}

type CommandCache = VecDeque<(String, Vec<u8>)>;

async fn handle_command_cached(
    cmd: &CommandMsg,
    allow_drop: bool,
    cache: &mut CommandCache,
) -> Option<Vec<u8>> {
    if let Some((_, response)) = cache.iter().find(|(cmd_id, _)| cmd_id == &cmd.cmd_id) {
        info!("duplicate command {}, returning cached result", cmd.cmd_id);
        return Some(response.clone());
    }

    let response = handle_command(cmd, allow_drop).await?;
    cache.push_back((cmd.cmd_id.clone(), response.clone()));
    if cache.len() > 32 {
        cache.pop_front();
    }
    Some(response)
}

fn collect_metrics(
    token: &str,
    cpu: &mut CpuSampler,
    io: &mut IoSampler,
) -> anyhow::Result<Vec<u8>> {
    let (resident, file_cache, anon) = parse_meminfo()?;
    let cpu_percent = cpu.sample();
    let io_rate = io.sample();
    let distro = std::env::var("WSL_DISTRO_NAME").unwrap_or_else(|_| "wsl".to_string());

    let msg = MetricsMsg {
        msg_type: "metrics",
        token,
        distro: &distro,
        resident,
        file_cache,
        anon,
        cpu_percent,
        io_rate,
    };
    Ok(serde_json::to_vec(&msg)?)
}

// ---------------------------------------------------------------------------
// Connection loop
// ---------------------------------------------------------------------------

async fn run_tcp_loop(
    mut stream: TcpStream,
    runtime: Arc<RwLock<GuestRuntimeConfig>>,
    host_last_cmd: Arc<Mutex<Option<Instant>>>,
    host_connected: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    host_connected.store(true, Ordering::Relaxed);
    let mut buf = vec![0u8; 8192];
    let mut cpu = CpuSampler::new();
    let mut io = IoSampler::new();
    let mut command_cache = CommandCache::new();
    let result = async {
        loop {
            let rt = current_runtime(&runtime);
            let out = collect_metrics(&rt.token, &mut cpu, &mut io)?;
            stream.write_all(&out).await?;

            match tokio::time::timeout(Duration::from_secs(1), stream.read(&mut buf)).await {
                Ok(Ok(0)) => return Err(anyhow::anyhow!("server closed connection")),
                Ok(Ok(n)) if n > 0 => {
                    if let Ok(cmd) = serde_json::from_slice::<CommandMsg>(&buf[..n]) {
                        if let Some(response) =
                            handle_command_cached(&cmd, rt.allow_drop, &mut command_cache).await
                        {
                            let _ = stream.write_all(&response).await;
                        }
                        if let Ok(mut t) = host_last_cmd.lock() {
                            *t = Some(Instant::now());
                        }
                    }
                }
                _ => {}
            }

            sleep(Duration::from_secs(rt.interval)).await;
        }
    }
    .await;
    host_connected.store(false, Ordering::Relaxed);
    result
}

// ---------------------------------------------------------------------------
// UDP connection loop — fire-and-forget metrics, short recv timeout for cmds
// ---------------------------------------------------------------------------

async fn run_udp_loop(
    host_addr: std::net::SocketAddr,
    runtime: Arc<RwLock<GuestRuntimeConfig>>,
    host_last_cmd: Arc<Mutex<Option<Instant>>>,
    host_connected: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    sock.connect(host_addr).await?;
    host_connected.store(true, Ordering::Relaxed);

    let mut buf = vec![0u8; 8192];
    let mut cpu = CpuSampler::new();
    let mut io = IoSampler::new();
    let mut command_cache = CommandCache::new();

    let result: anyhow::Result<()> = async {
        loop {
            let rt = current_runtime(&runtime);
            let out = collect_metrics(&rt.token, &mut cpu, &mut io)?;
            sock.send(&out).await?;

            match tokio::time::timeout(Duration::from_secs(1), sock.recv(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    if let Ok(cmd) = serde_json::from_slice::<CommandMsg>(&buf[..n]) {
                        if let Some(response) =
                            handle_command_cached(&cmd, rt.allow_drop, &mut command_cache).await
                        {
                            let _ = sock.send(&response).await;
                        }
                        if let Ok(mut t) = host_last_cmd.lock() {
                            *t = Some(Instant::now());
                        }
                    }
                }
                _ => {}
            }

            sleep(Duration::from_secs(rt.interval)).await;
        }
    }
    .await;

    host_connected.store(false, Ordering::Relaxed);
    result
}

// ---------------------------------------------------------------------------
// Local reclamation loop — runs alongside host connection.
//
// When the host is connected and issuing commands, this loop stays silent
// (via `host_defer`).  When the host is unreachable, it becomes the sole
// reclamation authority using guest-local metrics.
// ---------------------------------------------------------------------------

async fn local_reclaim_loop(
    host_last_cmd: Arc<Mutex<Option<Instant>>>,
    host_connected: Arc<AtomicBool>,
    _allow_drop: bool,
) {
    let cfg = GuestLocalConfig::default();
    let interval = cfg.check_interval;
    let mut reclaimer = GuestLocalReclaimer::new(cfg);
    let mut cpu = CpuSampler::new();
    let mut io = IoSampler::new();

    info!("local reclaim loop started (interval {:?})", interval);

    loop {
        sleep(interval).await;

        let metrics = match collect_local_metrics(&mut cpu, &mut io) {
            Some(m) => m,
            None => continue,
        };
        reclaimer.push(metrics);

        let hlc = host_last_cmd.lock().ok().and_then(|g| *g);
        let action = reclaimer.decide(hlc);

        match action {
            GuestLocalAction::Nothing => {}
            GuestLocalAction::Reclaim { bytes } => {
                let connected = host_connected.load(Ordering::Relaxed);
                info!(
                    "local reclaim: {} bytes (host_connected={})",
                    bytes, connected
                );
                match execute_reclaim(bytes).await {
                    Ok(freed) => info!("local reclaim freed ~{} bytes", freed),
                    Err(e) => warn!("local reclaim failed: {}", e),
                }
            }
        }
    }
}

fn collect_local_metrics(cpu: &mut CpuSampler, io: &mut IoSampler) -> Option<GuestLocalMetrics> {
    let s = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = 0u64;
    let mut available = 0u64;
    let mut cached = 0u64;
    let mut s_reclaim = 0u64;
    let mut buffers = 0u64;
    let mut free = 0u64;

    for line in s.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let val = parts[1].parse::<u64>().unwrap_or(0) * 1024;
        match parts[0] {
            "MemTotal:" => total = val,
            "MemAvailable:" => available = val,
            "MemFree:" => free = val,
            "Cached:" => cached = val,
            "SReclaimable:" => s_reclaim = val,
            "Buffers:" => buffers = val,
            _ => {}
        }
    }

    let file_cache = cached + s_reclaim + buffers;
    let resident = total.saturating_sub(free);

    Some(GuestLocalMetrics {
        mem_total: total,
        mem_available: available,
        file_cache,
        resident,
        cpu_percent: cpu.sample(),
        io_rate: io.sample(),
    })
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Opt::parse();

    // --- Service management (no tracing needed) ----------------------------
    if opt.install {
        return install_service(&opt.config);
    }
    if opt.uninstall {
        return uninstall_service();
    }
    if opt.status {
        return show_status();
    }
    if opt.check_port {
        println!("{}", diagnose_port_conflicts());
        return Ok(());
    }

    // --- Agent mode --------------------------------------------------------
    tracing_subscriber::fmt::init();

    // Probe kernel interfaces early so the log shows capabilities.
    let _ = caps();

    let cfg = GuestConfig::load(&opt.config)?.merged_with_cli(&opt);
    info!("loaded guest config from {}", opt.config.display());

    let runtime_initial = load_runtime_config(&opt.config, &opt).unwrap_or_else(|e| {
        error!("{}", e);
        std::process::exit(1);
    });

    if runtime_initial.allow_drop {
        warn!("drop_caches is ENABLED (may impact performance)");
    }

    let runtime = spawn_config_reloader(opt.config.clone(), opt.clone(), runtime_initial);

    // Shared state between host-connection loop and local reclaim loop.
    let host_last_cmd: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let host_connected = Arc::new(AtomicBool::new(false));

    // Spawn the local reclaim loop (runs forever alongside host connection).
    tokio::spawn(local_reclaim_loop(
        Arc::clone(&host_last_cmd),
        Arc::clone(&host_connected),
        cfg.allow_drop,
    ));

    // Resolve target address.
    let host_addr = resolve_host_addr(&cfg.host)?;
    info!(
        "target: {}  protocol: {}",
        host_addr,
        if cfg.tcp { "TCP" } else { "UDP" }
    );

    if cfg.tcp {
        // ---- TCP mode (fallback) ----
        if cfg.multi_path && cfg.host.starts_with("auto:") {
            let gateway_ip = discover_gateway_ip().unwrap_or_else(|| {
                error!("failed discover gateway ip");
                std::process::exit(1);
            });
            let targets = MultiPathConnector::build_guest_targets(RECOMMENDED_PORTS, &gateway_ip);
            info!("{} TCP connection targets prepared", targets.len());

            let config = MultiPathConfig {
                targets,
                connect_timeout: Duration::from_secs(3),
                max_retries: 3,
            };
            let connector = MultiPathConnector::new(config);

            loop {
                match connector.connect().await {
                    Ok((stream, target)) => {
                        info!(
                            "connected to {} (mode: {:?})",
                            target.socket_addr(),
                            target.mode
                        );
                        if let Err(e) = run_tcp_loop(
                            stream,
                            Arc::clone(&runtime),
                            Arc::clone(&host_last_cmd),
                            Arc::clone(&host_connected),
                        )
                        .await
                        {
                            warn!("TCP connection lost: {}", e);
                        }
                        sleep(Duration::from_secs(3)).await;
                    }
                    Err(e) => {
                        warn!("all TCP targets failed: {} -> retry in 10s", e);
                        sleep(Duration::from_secs(10)).await;
                    }
                }
            }
        } else {
            loop {
                match TcpStream::connect(&*host_addr).await {
                    Ok(stream) => {
                        info!("TCP connected to {}", host_addr);
                        if let Err(e) = run_tcp_loop(
                            stream,
                            Arc::clone(&runtime),
                            Arc::clone(&host_last_cmd),
                            Arc::clone(&host_connected),
                        )
                        .await
                        {
                            warn!("TCP connection lost: {}", e);
                        }
                        sleep(Duration::from_secs(3)).await;
                    }
                    Err(e) => {
                        warn!("TCP connect failed: {} -> retrying in 3s", e);
                        sleep(Duration::from_secs(3)).await;
                    }
                }
            }
        }
    } else {
        // ---- UDP mode (default) ----
        let addr: std::net::SocketAddr = host_addr.parse().unwrap_or_else(|e| {
            error!("invalid host addr for UDP: {} - {}", host_addr, e);
            std::process::exit(1);
        });

        if cfg.multi_path && cfg.host.starts_with("auto:") {
            let gateway_ip = discover_gateway_ip().unwrap_or_else(|| {
                error!("failed discover gateway ip");
                std::process::exit(1);
            });
            let connector = MultiPathConnector::new_udp(RECOMMENDED_PORTS, &gateway_ip);
            info!("UDP multi-path probe starting");

            loop {
                match connector.probe_udp().await {
                    Some(target_addr) => {
                        info!("UDP target reachable: {}", target_addr);
                        if let Err(e) = run_udp_loop(
                            target_addr,
                            Arc::clone(&runtime),
                            Arc::clone(&host_last_cmd),
                            Arc::clone(&host_connected),
                        )
                        .await
                        {
                            warn!("UDP loop error: {}", e);
                        }
                        sleep(Duration::from_secs(3)).await;
                    }
                    None => {
                        warn!("all UDP targets unreachable -> retry in 10s");
                        sleep(Duration::from_secs(10)).await;
                    }
                }
            }
        } else {
            loop {
                if let Err(e) = run_udp_loop(
                    addr,
                    Arc::clone(&runtime),
                    Arc::clone(&host_last_cmd),
                    Arc::clone(&host_connected),
                )
                .await
                {
                    warn!("UDP loop error: {} -> retrying in 3s", e);
                }
                sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

fn resolve_host_addr(host_arg: &str) -> anyhow::Result<String> {
    if host_arg.starts_with("auto:") {
        let parts: Vec<&str> = host_arg.splitn(2, ':').collect();
        let suffix = parts.get(1).copied().unwrap_or("multi");
        let port = suffix.parse::<u16>().unwrap_or(15555);
        match discover_gateway_ip() {
            Some(ip) => Ok(format!("{}:{}", ip, port)),
            None => Err(anyhow::anyhow!("failed to discover gateway ip")),
        }
    } else {
        Ok(host_arg.to_string())
    }
}
