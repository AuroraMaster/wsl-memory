use std::process::Command;

const RULE_NAME: &str = "WSL Memory Agent";

pub fn add_rule(ports: &[u16]) -> Result<(), String> {
    let ports_str: String = ports
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");

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
