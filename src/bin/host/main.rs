mod config;

#[cfg(windows)]
mod firewall;
#[cfg(windows)]
mod service;

use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tracing::{error, info, warn};
use wsl_memory_agent::{
    diagnose_port_conflicts, ElasticReclaimer, PortManager, PressureLevel, ReclamationAction,
    ReclamationConfig, SystemMetrics,
};

#[cfg(windows)]
use sysinfo::System;

#[cfg(windows)]
fn create_system() -> System {
    System::new_all()
}

#[cfg(not(windows))]
struct System;

#[cfg(not(windows))]
fn create_system() -> System {
    System
}

#[derive(Parser, Debug)]
#[command(about = "WSL Memory Host Agent — manage WSL2 vmmem memory")]
struct Opt {
    #[arg(long, default_value = "0.0.0.0:15555")]
    listen: String,

    #[arg(long)]
    token: Option<PathBuf>,

    #[arg(long)]
    auto_port: bool,

    #[arg(long)]
    check_port: bool,

    /// Run as a Windows Service (called by SCM)
    #[arg(long)]
    service: bool,

    /// Install as a Windows Service
    #[arg(long)]
    install: bool,

    /// Uninstall the Windows Service
    #[arg(long)]
    uninstall: bool,

    /// Use TCP instead of UDP (fallback for unusual network configs)
    #[arg(long)]
    tcp: bool,
}

// ---------------------------------------------------------------------------
// Protocol types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GuestMetricsMsg {
    #[allow(dead_code)]
    msg_type: String,
    #[allow(dead_code)]
    token: Option<String>,
    distro: Option<String>,
    resident: u64,
    file_cache: u64,
    cpu_percent: Option<f32>,
    io_rate: Option<f32>,
}

#[derive(Serialize)]
struct CommandMsg {
    msg_type: String,
    cmd_id: String,
    action: String,
    bytes: Option<u64>,
    level: Option<u8>,
    reason: String,
    pressure_level: Option<String>,
}

struct ConnectionState {
    reclaimer: ElasticReclaimer,
    distro_name: String,
}

// ---------------------------------------------------------------------------
// Platform-specific helpers
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn get_system_info(sys: &mut System) -> (u64, u64, u64) {
    sys.refresh_processes();
    sys.refresh_memory();

    let vmmem = sys
        .processes()
        .values()
        .find(|p| p.name().eq_ignore_ascii_case("vmmem"));

    let vmmem_rss = vmmem.map(|p| p.memory()).unwrap_or(0);
    let total_mem = sys.total_memory();
    let avail_mem = sys.available_memory();

    (vmmem_rss, total_mem, avail_mem)
}

#[cfg(not(windows))]
fn get_system_info(_sys: &mut System) -> (u64, u64, u64) {
    warn!("vmmem info only available on Windows");
    (0, 0, 0)
}

#[cfg(windows)]
fn get_wslconfig_memory_limit() -> Option<u64> {
    let home = std::env::var("USERPROFILE").ok()?;
    let content = std::fs::read_to_string(PathBuf::from(home).join(".wslconfig")).ok()?;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("memory=") || line.starts_with("memory =") {
            let value = line.split('=').nth(1)?.trim();
            if let Some(gb) = value.strip_suffix("GB").or(value.strip_suffix("gb")) {
                return gb
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .map(|v| v * 1024 * 1024 * 1024);
            } else if let Some(mb) = value.strip_suffix("MB").or(value.strip_suffix("mb")) {
                return mb.trim().parse::<u64>().ok().map(|v| v * 1024 * 1024);
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn get_wslconfig_memory_limit() -> Option<u64> {
    None
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn generate_cmd_id() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos()
        .to_string()
}

fn validate_token(val: &serde_json::Value, expected: &Option<String>) -> bool {
    if let Some(expected) = expected {
        match val.get("token").and_then(|v| v.as_str()) {
            Some(t) if t == expected => true,
            Some(_) => {
                warn!("token mismatch; closing");
                false
            }
            None => {
                warn!("no token provided; closing");
                false
            }
        }
    } else {
        true
    }
}

async fn send_command_tcp(stream: &mut TcpStream, cmd: &CommandMsg) -> bool {
    match serde_json::to_vec(cmd) {
        Ok(out) => {
            if let Err(e) = stream.write_all(&out).await {
                warn!("failed send TCP command: {}", e);
                false
            } else {
                true
            }
        }
        Err(e) => {
            warn!("failed serialize command: {}", e);
            false
        }
    }
}

async fn send_command_udp(sock: &UdpSocket, addr: SocketAddr, cmd: &CommandMsg) -> bool {
    match serde_json::to_vec(cmd) {
        Ok(out) => {
            if let Err(e) = sock.send_to(&out, addr).await {
                warn!("failed send UDP command to {}: {}", addr, e);
                false
            } else {
                true
            }
        }
        Err(e) => {
            warn!("failed serialize command: {}", e);
            false
        }
    }
}

fn build_action_command(
    action: ReclamationAction,
    diagnostics: &str,
    pressure: PressureLevel,
) -> Option<CommandMsg> {
    match action {
        ReclamationAction::NoAction => None,
        ReclamationAction::GradualReclaim { bytes } => {
            info!("sending reclaim {} bytes (pressure: {:?})", bytes, pressure);
            Some(CommandMsg {
                msg_type: "command".into(),
                cmd_id: generate_cmd_id(),
                action: "reclaim".into(),
                bytes: Some(bytes),
                level: None,
                reason: format!("elastic reclaim: {}", diagnostics),
                pressure_level: Some(format!("{:?}", pressure)),
            })
        }
        ReclamationAction::Compact => {
            info!("sending compact command");
            Some(CommandMsg {
                msg_type: "command".into(),
                cmd_id: generate_cmd_id(),
                action: "compact".into(),
                bytes: None,
                level: None,
                reason: format!("memory compaction: {}", diagnostics),
                pressure_level: Some("Heavy".into()),
            })
        }
        ReclamationAction::DropCaches { level } => {
            warn!("sending drop_caches level {} (CRITICAL)", level);
            Some(CommandMsg {
                msg_type: "command".into(),
                cmd_id: generate_cmd_id(),
                action: "drop_caches".into(),
                bytes: None,
                level: Some(level),
                reason: format!("CRITICAL pressure: {}", diagnostics),
                pressure_level: Some("Critical".into()),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handlers
// ---------------------------------------------------------------------------

async fn handle_connection_elastic(
    mut stream: TcpStream,
    token: Option<String>,
    reclaim_config: ReclamationConfig,
) {
    let peer = stream.peer_addr().ok();
    info!("accepted TCP connection from {:?}", peer);

    let mut sys = create_system();
    let mut state = ConnectionState {
        reclaimer: ElasticReclaimer::new(reclaim_config),
        distro_name: String::from("unknown"),
    };

    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) => {
                info!("connection closed by peer {:?}", peer);
                return;
            }
            Ok(n) => n,
            Err(e) => {
                error!("read error: {}", e);
                return;
            }
        };

        let s = match std::str::from_utf8(&buf[..n]) {
            Ok(s) => s,
            Err(e) => {
                warn!("invalid utf8: {}", e);
                continue;
            }
        };

        let val = match serde_json::from_str::<serde_json::Value>(s) {
            Ok(v) => v,
            Err(e) => {
                warn!("json parse error: {}", e);
                continue;
            }
        };

        match val.get("msg_type").and_then(|v| v.as_str()) {
            Some("metrics") => {
                if !validate_token(&val, &token) {
                    let _ = stream.shutdown().await;
                    return;
                }

                let m: GuestMetricsMsg = match serde_json::from_value(val) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("metrics parse error: {}", e);
                        continue;
                    }
                };

                if let Some(cmd) = process_elastic_metrics(&m, &mut state, &mut sys) {
                    send_command_tcp(&mut stream, &cmd).await;
                }
            }
            Some("result") => {
                info!("received result: {}", s);
            }
            _ => {}
        }
    }
}

/// Shared logic: process a metrics message through the elastic algorithm.
fn process_elastic_metrics(
    m: &GuestMetricsMsg,
    state: &mut ConnectionState,
    sys: &mut System,
) -> Option<CommandMsg> {
    state.distro_name = m.distro.clone().unwrap_or_else(|| "unknown".to_string());
    let (vmmem_rss, host_total, host_avail) = get_system_info(sys);
    let wslconfig_limit = get_wslconfig_memory_limit();
    let gap = vmmem_rss.saturating_sub(m.resident);

    let metrics = SystemMetrics {
        vmmem_rss,
        host_memory_total: host_total,
        host_memory_avail: host_avail,
        wslconfig_memory_limit: wslconfig_limit,
        guest_resident: m.resident,
        guest_file_cache: m.file_cache,
        guest_cpu_percent: m.cpu_percent.unwrap_or(0.0) / 100.0,
        guest_io_rate: m.io_rate.unwrap_or(0.0),
        gap,
    };

    info!(
        "metrics from {} gap={:.2}GB vmmem={:.2}GB guest={:.2}GB cache={:.2}GB",
        state.distro_name,
        gap as f64 / 1024.0 / 1024.0 / 1024.0,
        vmmem_rss as f64 / 1024.0 / 1024.0 / 1024.0,
        m.resident as f64 / 1024.0 / 1024.0 / 1024.0,
        m.file_cache as f64 / 1024.0 / 1024.0 / 1024.0,
    );

    state.reclaimer.push_metrics(metrics);
    let action = state.reclaimer.decide_action();
    let diagnostics = state.reclaimer.get_diagnostics();
    let pressure = state.reclaimer.calculate_pressure_level();
    info!("diagnostics: {}", diagnostics);

    build_action_command(action, &diagnostics, pressure)
}

// ---------------------------------------------------------------------------
// Per-guest state for UDP mode
// ---------------------------------------------------------------------------

struct UdpGuestState {
    state: ConnectionState,
    /// Last command sent — resend if no ack received by next tick.
    pending_cmd: Option<CommandMsg>,
}

// ---------------------------------------------------------------------------
// UDP server loop
// ---------------------------------------------------------------------------

async fn run_udp_server(listen_addr: &str, token: Option<String>) -> anyhow::Result<()> {
    let sock = UdpSocket::bind(listen_addr).await?;
    info!("UDP server listening on {}", listen_addr);

    let reclaim_config = ReclamationConfig::default();
    let mut sys = create_system();

    let mut guests: HashMap<SocketAddr, UdpGuestState> = HashMap::new();
    let mut buf = vec![0u8; 16 * 1024];

    loop {
        let (n, src) = match sock.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                warn!("UDP recv error: {}", e);
                continue;
            }
        };

        let s = match std::str::from_utf8(&buf[..n]) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let val = match serde_json::from_str::<serde_json::Value>(s) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match val.get("msg_type").and_then(|v| v.as_str()) {
            Some("ping") => {
                let _ = sock.send_to(b"{\"msg_type\":\"pong\"}", src).await;
                continue;
            }
            Some("result") => {
                info!("received result from {}: {}", src, s);
                if let Some(gs) = guests.get_mut(&src) {
                    gs.pending_cmd = None;
                }
                continue;
            }
            Some("metrics") => {}
            _ => continue,
        }

        if !validate_token(&val, &token) {
            continue;
        }

        let m: GuestMetricsMsg = match serde_json::from_value(val) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let gs = guests.entry(src).or_insert_with(|| UdpGuestState {
            state: ConnectionState {
                reclaimer: ElasticReclaimer::new(reclaim_config.clone()),
                distro_name: String::from("unknown"),
            },
            pending_cmd: None,
        });

        // Resend pending command if guest didn't ack.
        if let Some(ref cmd) = gs.pending_cmd {
            let _ = send_command_udp(&sock, src, cmd).await;
        }

        let cmd = process_elastic_metrics(&m, &mut gs.state, &mut sys);

        if let Some(cmd) = cmd {
            send_command_udp(&sock, src, &cmd).await;
            gs.pending_cmd = Some(cmd);
        }
    }
}

// ---------------------------------------------------------------------------
// Server entry point (shared between foreground and service modes)
// ---------------------------------------------------------------------------

pub async fn run_server(cfg: &config::HostConfig, use_tcp: bool) -> anyhow::Result<()> {
    let listen_addr = &cfg.listen_addr;

    let token = std::fs::read_to_string(&cfg.token_path)
        .ok()
        .map(|t| t.trim().to_string());

    info!("token configured: {}", token.is_some());
    info!("protocol: {}", if use_tcp { "TCP" } else { "UDP" });

    if use_tcp {
        let listener = TcpListener::bind(listen_addr).await?;
        info!("TCP server listening on {}", listen_addr);

        let reclaim_config = ReclamationConfig::default();

        loop {
            let (stream, _) = listener.accept().await?;
            let token = token.clone();

            let rc = reclaim_config.clone();
            tokio::spawn(handle_connection_elastic(stream, token, rc));
        }
    } else {
        run_udp_server(listen_addr, token).await
    }
}

// ---------------------------------------------------------------------------
// Main — mode dispatch
// ---------------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    let opt = Opt::parse();

    // --- Windows Service mode (must be dispatched before any I/O) ----------
    #[cfg(windows)]
    if opt.service {
        return service::run_as_service();
    }

    // --- Port diagnostics --------------------------------------------------
    if opt.check_port {
        println!("{}", diagnose_port_conflicts());
        return Ok(());
    }

    // --- Windows-only modes ------------------------------------------------
    #[cfg(windows)]
    {
        if opt.install {
            tracing_subscriber::fmt::init();
            info!("installing service");
            return service::install();
        }

        if opt.uninstall {
            tracing_subscriber::fmt::init();
            info!("uninstalling service");
            return service::uninstall();
        }
    }

    #[cfg(not(windows))]
    if opt.install || opt.uninstall || opt.service {
        eprintln!("--install / --uninstall / --service are only available on Windows");
        std::process::exit(1);
    }

    // --- Foreground mode ---------------------------------------------------
    tracing_subscriber::fmt::init();

    let cfg = if let Some(saved) = config::load() {
        info!("loaded config from {}", config::config_path().display());
        config::HostConfig {
            listen_addr: if opt.listen != "0.0.0.0:15555" {
                opt.listen.clone()
            } else {
                saved.listen_addr
            },
            token_path: opt.token.clone().unwrap_or(saved.token_path),
        }
    } else {
        config::HostConfig {
            listen_addr: if opt.auto_port {
                let port = PortManager::select_best_port();
                info!("auto-selected port: {}", port);
                format!("0.0.0.0:{}", port)
            } else {
                opt.listen.clone()
            },
            token_path: opt.token.unwrap_or_else(|| {
                #[cfg(windows)]
                {
                    PathBuf::from(r"C:\Users\Public\wsl_agent_token")
                }
                #[cfg(not(windows))]
                {
                    PathBuf::from("/tmp/wsl_agent_token")
                }
            }),
        }
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_server(&cfg, opt.tcp))
}
