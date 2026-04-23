#![cfg_attr(not(windows), allow(dead_code))]

use serde::{Deserialize, Serialize};
use std::net::{TcpListener, UdpSocket};
use std::path::PathBuf;
use wsl_memory_agent::ReclamationConfig;

use super::logging::HostLoggingConfig;

const APP_NAME: &str = "WSLMemoryAgent";
const DEFAULT_LISTEN_IP: &str = "0.0.0.0";
const DEFAULT_LISTEN_PORT: u16 = 15555;
const RECOMMENDED_PORTS: &[u16] = &[15555, 15556, 25555, 35555, 45555, 5555];
const DEFAULT_REMOTE_IPS: &[&str] = &["127.0.0.1", "::1", "172.16.0.0/12"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HostConfig {
    pub listen_ip: String,
    pub listen_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen_addr: Option<String>,
    pub token_path: PathBuf,
    #[serde(skip)]
    pub token_path_locked: bool,
    pub remote_ips: Vec<String>,
    pub reclamation: ReclamationConfig,
    pub logging: HostLoggingConfig,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            listen_ip: DEFAULT_LISTEN_IP.to_string(),
            listen_port: DEFAULT_LISTEN_PORT,
            listen_addr: None,
            token_path: default_token_path(),
            token_path_locked: false,
            remote_ips: DEFAULT_REMOTE_IPS.iter().map(|s| s.to_string()).collect(),
            reclamation: ReclamationConfig::default(),
            logging: HostLoggingConfig::default(),
        }
    }
}

impl HostConfig {
    pub fn effective_listen_addr(&self) -> String {
        self.listen_addr
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.listen_ip, self.listen_port))
    }

    pub fn effective_listen_port(&self) -> Option<u16> {
        self.effective_listen_addr()
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
    }
}

pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_else(|_| r"C:\ProgramData".to_string());
        PathBuf::from(appdata).join(APP_NAME)
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(".config").join("wsl-memory-agent")
    }
}

fn default_token_path() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(r"C:\Users\Public\wsl_agent_token")
    }
    #[cfg(not(windows))]
    {
        config_dir().join("token")
    }
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.yaml")
}

pub fn load() -> Option<HostConfig> {
    let content = std::fs::read_to_string(config_path()).ok()?;
    serde_yml::from_str(&content).ok()
}

pub fn load_or_create() -> anyhow::Result<HostConfig> {
    if let Some(cfg) = load() {
        return Ok(cfg);
    }
    let cfg = HostConfig {
        listen_port: select_available_port(),
        ..HostConfig::default()
    };
    save(&cfg)?;
    Ok(cfg)
}

pub fn save(config: &HostConfig) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_yml::to_string(config)?;
    std::fs::write(path, content)?;
    Ok(())
}

pub fn generate_token() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

#[allow(dead_code)]
pub fn ensure_token(path: &PathBuf) -> anyhow::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let token = generate_token();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &token)?;
    Ok(token)
}

fn select_available_port() -> u16 {
    RECOMMENDED_PORTS
        .iter()
        .copied()
        .find(|&port| port_available(port))
        .unwrap_or(DEFAULT_LISTEN_PORT)
}

fn port_available(port: u16) -> bool {
    TcpListener::bind((DEFAULT_LISTEN_IP, port)).is_ok()
        && UdpSocket::bind((DEFAULT_LISTEN_IP, port)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::HostConfig;
    use std::path::PathBuf;

    #[test]
    fn parses_yaml_host_config() {
        let yaml = r#"
listen_addr: "0.0.0.0:15555"
token_path: 'C:\Users\Public\wsl_agent_token'
"#;
        let cfg: HostConfig = serde_yml::from_str(yaml).expect("valid yaml host config");
        assert_eq!(cfg.effective_listen_addr(), "0.0.0.0:15555");
        assert_eq!(
            cfg.token_path,
            PathBuf::from(r"C:\Users\Public\wsl_agent_token")
        );
    }

    #[test]
    fn parses_split_ip_port_config() {
        let yaml = r#"
listen_ip: "127.0.0.1"
listen_port: 15556
token_path: 'C:\Users\Public\wsl_agent_token'
"#;
        let cfg: HostConfig = serde_yml::from_str(yaml).expect("valid yaml host config");
        assert_eq!(cfg.effective_listen_addr(), "127.0.0.1:15556");
    }
}
