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

/// Map the places this server talks about onto IANA zones. The system tz
/// database (`/etc/zoneinfo`) does the DST math, via `date`, no crate needed.
fn zone_for(place: &str) -> Option<&'static str> {
    let p = place.trim().to_lowercase();
    let p = p.trim_start_matches("in ").trim_start_matches("the ");
    let table: &[(&[&str], &str)] = &[
        (&["sweden", "stockholm", "sverige", "gothenburg", "göteborg", "cet", "cest", "here"], "Europe/Stockholm"),
        (&["colombia", "bogota", "bogotá", "medellin"], "America/Bogota"),
        (&["california", "la", "los angeles", "pst", "pdt", "pacific", "san francisco", "seattle"], "America/Los_Angeles"),
        (&["new york", "nyc", "est", "edt", "eastern", "florida", "miami", "boston"], "America/New_York"),
        (&["texas", "chicago", "cst", "central us"], "America/Chicago"),
        (&["denver", "colorado", "mst", "mountain"], "America/Denver"),
        (&["prague", "czech", "czechia"], "Europe/Prague"),
        (&["netherlands", "amsterdam", "holland", "dutch"], "Europe/Amsterdam"),
        (&["uk", "london", "england", "britain", "gmt", "bst"], "Europe/London"),
        (&["germany", "berlin"], "Europe/Berlin"),
        (&["denmark", "copenhagen"], "Europe/Copenhagen"),
        (&["norway", "oslo"], "Europe/Oslo"),
        (&["finland", "helsinki"], "Europe/Helsinki"),
        (&["spain", "madrid"], "Europe/Madrid"),
        (&["france", "paris"], "Europe/Paris"),
        (&["italy", "rome"], "Europe/Rome"),
        (&["poland", "warsaw"], "Europe/Warsaw"),
        (&["hong kong", "hk"], "Asia/Hong_Kong"),
        (&["japan", "tokyo", "jst"], "Asia/Tokyo"),
        (&["india", "ist", "delhi", "mumbai"], "Asia/Kolkata"),
        (&["china", "beijing", "shanghai"], "Asia/Shanghai"),
        (&["korea", "seoul"], "Asia/Seoul"),
        (&["australia", "sydney", "melbourne", "aest"], "Australia/Sydney"),
        (&["brazil", "sao paulo", "são paulo"], "America/Sao_Paulo"),
        (&["mexico", "mexico city"], "America/Mexico_City"),
        (&["utc", "gmt+0", "zulu"], "UTC"),
    ];
    for (names, zone) in table {
        if names.iter().any(|n| *n == p) {
            return Some(zone);
        }
    }
    // Accept a raw IANA name like Europe/Stockholm.
    if place.contains('/') && place.chars().all(|c| c.is_alphanumeric() || c == '/' || c == '_' || c == '-') {
        return Some(Box::leak(place.to_string().into_boxed_str()));
    }
    None
}

/// Current time somewhere else, via the system tz database.
pub fn time_in(place: &str) -> Result<String, String> {
    let Some(zone) = zone_for(place) else {
        return Err(format!("time_in error: don't know the time zone for '{}' (try a country, city or Europe/City)", place.trim()));
    };
    if !std::path::Path::new(&format!("/etc/zoneinfo/{zone}")).exists() && !std::path::Path::new(&format!("/usr/share/zoneinfo/{zone}")).exists() && zone != "UTC" {
        return Err(format!("time_in error: no tz data for {zone}"));
    }
    let out = std::process::Command::new("date")
        .env("TZ", zone)
        .arg("+%A %Y-%m-%d %H:%M %Z (UTC%:z)")
        .output()
        .map_err(|e| format!("time_in error: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return Err("time_in error: date printed nothing".to_string());
    }
    let here = chrono::Local::now().format("%H:%M %Z").to_string();
    Ok(format!("{}: {text}. here (Sweden) it is {here}.", place.trim()))
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

    #[test]
    fn time_in_known_places() {
        let t = time_in("colombia").unwrap();
        assert!(t.contains("-05") || t.contains("UTC-05"), "{t}");
        assert!(time_in("Europe/Prague").is_ok());
        assert!(time_in("narnia").is_err());
    }

    #[tokio::test]
    async fn runs_a_command() {
        let out = run_command("echo hello", false, None).await.unwrap();
        assert!(out.contains("hello") && out.contains("(exit 0)"));
        assert!(run_command("", false, None).await.is_err());
    }
}
