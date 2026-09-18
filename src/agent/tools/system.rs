//! Clock + owner-only shell access on the bot host.

use std::time::Duration;

/// Local + UTC time, day of year and ISO week.
pub fn get_time() -> String {
    let now = chrono::Local::now();
    let utc = chrono::Utc::now();
    format!(
        "Local time: {} (UTC: {}). Day of year {}, ISO week {}.",
        now.format("%A, %Y-%m-%d %H:%M:%S %Z"),
        utc.format("%Y-%m-%d %H:%M:%S"),
        now.format("%j"),
        now.format("%V"),
    )
}

/// Compact "Thursday 2026-09-18 08:41 CEST" for prompts.
pub fn now_line() -> String {
    chrono::Local::now().format("%A %Y-%m-%d %H:%M %Z").to_string()
}

/// Run a shell command on this machine (the bot host). The caller enforces
/// the owner gate; the sudo password never reaches the model (it is scrubbed
/// from the input before the prompt is built and passed here out-of-band).
pub async fn run_command(command: &str, sudo: bool, sudo_password: Option<&str>) -> Result<String, String> {
    let command = command.trim();
    if command.is_empty() {
        return Err("run_command error: no command".to_string());
    }
    let use_sudo = sudo && sudo_password.is_some();
    let mut cmd = if use_sudo {
        let mut c = tokio::process::Command::new("sudo");
        c.args(["-S", "-p", "", "bash", "-lc", command]);
        c.stdin(std::process::Stdio::piped());
        c
    } else {
        let mut c = tokio::process::Command::new("bash");
        c.args(["-lc", command]);
        c.stdin(std::process::Stdio::null());
        c
    };
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("run_command error: {e}"))?;
    if use_sudo {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(format!("{}\n", sudo_password.unwrap_or("")).as_bytes()).await;
        }
    }
    let output = match tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("run_command error: {e}")),
        Err(_) => return Err(format!("run_command: '{command}' timed out after 60s")),
    };
    let mut out = String::from_utf8_lossy(&output.stdout).to_string();
    let err = String::from_utf8_lossy(&output.stderr);
    if !err.trim().is_empty() {
        out.push_str("\n[stderr]\n");
        out.push_str(&err);
    }
    let out = out.trim().to_string();
    let out = if out.chars().count() > 2500 {
        format!("{}\n…(truncated)", out.chars().take(2500).collect::<String>())
    } else {
        out
    };
    let code = output.status.code().unwrap_or(-1);
    let prefix = if use_sudo { "sudo " } else { "" };
    Ok(format!(
        "$ {prefix}{command}\n(exit {code})\n{}",
        if out.is_empty() { "(no output)" } else { &out }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_has_both_zones() {
        let t = get_time();
        assert!(t.contains("Local time:") && t.contains("UTC:"));
    }

    #[tokio::test]
    async fn runs_a_command() {
        let out = run_command("echo hello", false, None).await.unwrap();
        assert!(out.contains("hello") && out.contains("(exit 0)"));
        assert!(run_command("", false, None).await.is_err());
    }
}
