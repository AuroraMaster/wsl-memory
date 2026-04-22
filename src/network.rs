// 网络通讯冗余与端口管理模块

use serde::{Deserialize, Serialize};
use std::net::{SocketAddr, TcpListener};
use std::time::Duration;
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;

/// 推荐的端口列表（按优先级排序，避开常用端口）
pub const RECOMMENDED_PORTS: &[u16] = &[
    15555, // 主端口（冷门）
    15556, // 备用端口 1
    25555, // 备用端口 2
    35555, // 备用端口 3
    45555, // 备用端口 4
    5555,  // 传统端口（可能冲突）
];

/// 连接模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionMode {
    /// 直连模式（0.0.0.0）
    Direct,
    /// NAT 模式（通过网关）
    Nat,
}

/// 连接目标
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionTarget {
    pub host: String,
    pub port: u16,
    pub mode: ConnectionMode,
    pub priority: u8, // 优先级（数字越小越优先）
}

impl ConnectionTarget {
    pub fn socket_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// 多路径连接配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiPathConfig {
    pub targets: Vec<ConnectionTarget>,
    pub connect_timeout: Duration,
    pub max_retries: usize,
}

impl Default for MultiPathConfig {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            connect_timeout: Duration::from_secs(3),
            max_retries: 3,
        }
    }
}

/// 端口可用性检查结果
#[derive(Debug)]
pub struct PortCheckResult {
    pub port: u16,
    pub available: bool,
    pub error: Option<String>,
}

/// 端口管理器
pub struct PortManager;

impl PortManager {
    /// 检查端口是否可用（可以绑定）
    pub fn is_port_available(port: u16) -> bool {
        TcpListener::bind(format!("0.0.0.0:{}", port)).is_ok()
    }

    /// 批量检查端口
    pub fn check_ports(ports: &[u16]) -> Vec<PortCheckResult> {
        ports
            .iter()
            .map(|&port| {
                let available = Self::is_port_available(port);
                let error = if !available {
                    Some(format!("端口 {} 已被占用", port))
                } else {
                    None
                };
                PortCheckResult {
                    port,
                    available,
                    error,
                }
            })
            .collect()
    }

    /// 找到第一个可用的端口
    pub fn find_available_port(ports: &[u16]) -> Option<u16> {
        ports
            .iter()
            .copied()
            .find(|&port| Self::is_port_available(port))
    }

    /// 智能选择端口（优先冷门端口，避免冲突）
    pub fn select_best_port() -> u16 {
        // 首先尝试推荐的冷门端口
        if let Some(port) = Self::find_available_port(RECOMMENDED_PORTS) {
            return port;
        }

        // 如果都被占用，尝试随机冷门端口范围（10000-65535）
        for _ in 0..10 {
            let port = 10000 + (rand::random::<u16>() % 55535);
            if Self::is_port_available(port) {
                return port;
            }
        }

        // 最后降级到推荐列表第一个（即使被占用）
        RECOMMENDED_PORTS[0]
    }
}

/// 多路径连接器 — supports both TCP and UDP probe modes.
pub struct MultiPathConnector {
    config: MultiPathConfig,
    /// Addresses for UDP probe (built by `new_udp`).
    udp_targets: Vec<SocketAddr>,
}

impl MultiPathConnector {
    pub fn new(config: MultiPathConfig) -> Self {
        Self {
            config,
            udp_targets: Vec::new(),
        }
    }

    /// Build a UDP-only connector that probes the given ports on the gateway.
    pub fn new_udp(ports: &[u16], gateway_ip: &str) -> Self {
        let addrs: Vec<SocketAddr> = ports
            .iter()
            .filter_map(|&p| format!("{}:{}", gateway_ip, p).parse().ok())
            .collect();
        Self {
            config: MultiPathConfig::default(),
            udp_targets: addrs,
        }
    }

    /// Probe UDP targets: send a tiny ping and see who answers first.
    /// Returns the first responsive address.
    pub async fn probe_udp(&self) -> Option<SocketAddr> {
        let sock = UdpSocket::bind("0.0.0.0:0").await.ok()?;
        let ping = b"{\"msg_type\":\"ping\"}";

        for addr in &self.udp_targets {
            let _ = sock.send_to(ping, addr).await;
        }

        let mut buf = [0u8; 512];
        match timeout(Duration::from_secs(3), sock.recv_from(&mut buf)).await {
            Ok(Ok((_n, src))) => Some(src),
            _ => None,
        }
    }

    /// 自动构建 Guest 端的多路径配置
    pub fn build_guest_targets(ports: &[u16], gateway_ip: &str) -> Vec<ConnectionTarget> {
        let mut targets = Vec::new();
        let mut priority = 0u8;

        // 方式 1: 通过网关 IP（NAT 模式，最常用）
        for &port in ports {
            targets.push(ConnectionTarget {
                host: gateway_ip.to_string(),
                port,
                mode: ConnectionMode::Nat,
                priority,
            });
            priority += 1;
        }

        // 方式 2: 尝试常见的 WSL2 host IP 范围
        let common_hosts = vec![
            "172.16.0.1",
            "172.17.0.1",
            "172.18.0.1",
            "172.19.0.1",
            "172.20.0.1",
            "172.21.0.1",
            "172.22.0.1",
            "172.23.0.1",
            "172.24.0.1",
            "172.25.0.1",
            "172.26.0.1",
            "172.27.0.1",
            "172.28.0.1",
            "172.29.0.1",
            "172.30.0.1",
            "172.31.0.1",
        ];

        for host in common_hosts {
            if host != gateway_ip {
                // 只尝试主端口（避免太多连接尝试）
                targets.push(ConnectionTarget {
                    host: host.to_string(),
                    port: ports[0],
                    mode: ConnectionMode::Nat,
                    priority,
                });
                priority += 1;
            }
        }

        targets
    }

    /// 尝试连接（按优先级顺序）
    pub async fn connect(&self) -> Result<(TcpStream, ConnectionTarget), String> {
        let mut targets = self.config.targets.clone();
        targets.sort_by_key(|t| t.priority);

        for target in targets {
            for retry in 0..self.config.max_retries {
                match timeout(
                    self.config.connect_timeout,
                    TcpStream::connect(target.socket_addr()),
                )
                .await
                {
                    Ok(Ok(stream)) => {
                        tracing::info!(
                            "成功连接到 {} (mode: {:?}, priority: {}, retry: {})",
                            target.socket_addr(),
                            target.mode,
                            target.priority,
                            retry
                        );
                        return Ok((stream, target));
                    }
                    Ok(Err(e)) => {
                        tracing::debug!(
                            "连接 {} 失败: {} (retry {}/{})",
                            target.socket_addr(),
                            e,
                            retry + 1,
                            self.config.max_retries
                        );
                    }
                    Err(_) => {
                        tracing::debug!(
                            "连接 {} 超时 (retry {}/{})",
                            target.socket_addr(),
                            retry + 1,
                            self.config.max_retries
                        );
                    }
                }

                if retry < self.config.max_retries - 1 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }

        Err("所有连接目标都失败".to_string())
    }
}

/// 端口冲突检测和建议
pub fn diagnose_port_conflicts() -> String {
    let mut report = String::new();
    report.push_str("=== 端口可用性诊断 ===\n\n");

    let results = PortManager::check_ports(RECOMMENDED_PORTS);

    for result in &results {
        let status = if result.available {
            "✓ 可用"
        } else {
            "✗ 占用"
        };
        report.push_str(&format!("端口 {}: {}\n", result.port, status));
        if let Some(err) = &result.error {
            report.push_str(&format!("  原因: {}\n", err));
        }
    }

    let available_count = results.iter().filter(|r| r.available).count();
    report.push_str(&format!(
        "\n可用端口: {}/{}\n",
        available_count,
        results.len()
    ));

    if available_count == 0 {
        report.push_str("\n⚠️  警告: 所有推荐端口都被占用！\n");
        report.push_str("建议: 使用自动端口选择功能\n");

        let auto_port = PortManager::select_best_port();
        report.push_str(&format!("推荐使用端口: {}\n", auto_port));
    } else {
        let first_available = results.iter().find(|r| r.available).unwrap();
        report.push_str(&format!("\n推荐使用端口: {}\n", first_available.port));
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_availability() {
        // 测试常见端口
        let results = PortManager::check_ports(&[80, 443, 5555]);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_find_available_port() {
        // 应该能找到至少一个可用端口
        let port = PortManager::find_available_port(RECOMMENDED_PORTS);
        assert!(port.is_some());
    }

    #[test]
    fn test_select_best_port() {
        let port = PortManager::select_best_port();
        assert!(port > 0);
    }

    #[test]
    fn test_build_guest_targets() {
        let targets = MultiPathConnector::build_guest_targets(&[15555, 15556], "172.28.112.1");
        assert!(!targets.is_empty());

        // 第一个目标应该是网关 IP
        assert_eq!(targets[0].host, "172.28.112.1");
        assert_eq!(targets[0].mode, ConnectionMode::Nat);
    }
}
