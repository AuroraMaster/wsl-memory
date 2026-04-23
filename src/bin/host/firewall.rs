use std::process::Command;

const RULE_NAME: &str = "WSL Memory Agent";

pub fn add_rule(ports: &[u16], remote_ips: &[String]) -> Result<(), String> {
    let ports_str: String = ports
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let remote_ips = if remote_ips.is_empty() {
        "127.0.0.1,::1,172.16.0.0/12".to_string()
    } else {
        remote_ips.join(",")
    };

    for protocol in ["TCP", "UDP"] {
        let rule_name = format!("{} {}", RULE_NAME, protocol);
        let status = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name={}", rule_name),
                "dir=in",
                "action=allow",
                &format!("protocol={}", protocol),
                &format!("localport={}", ports_str),
                &format!("remoteip={}", remote_ips),
            ])
            .status()
            .map_err(|e| format!("netsh failed: {}", e))?;

        if !status.success() {
            return Err(format!("netsh returned exit code: {}", status));
        }
    }

    Ok(())
}

pub fn remove_rule() -> Result<(), String> {
    for protocol in ["TCP", "UDP"] {
        let rule_name = format!("{} {}", RULE_NAME, protocol);
        let status = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={}", rule_name),
            ])
            .status()
            .map_err(|e| format!("netsh failed: {}", e))?;

        if !status.success() {
            return Err(format!("netsh returned exit code: {}", status));
        }
    }

    Ok(())
}
