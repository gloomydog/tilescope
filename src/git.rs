//! git log から各ファイルの最終コミット時刻とコミット回数を集計する

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

pub struct GitStats {
    /// root からの相対パス ('/' 区切り) → (最終コミット unix 時刻, コミット回数)
    pub files: HashMap<String, (i64, u32)>,
    pub commits: usize,
    pub truncated: bool,
}

pub fn load(root: &Path, max_commits: usize) -> Result<GitStats, String> {
    let out = Command::new("git")
        .arg("-C").arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(Stdio::null())
        .output()
        .map_err(|_| "git not found".to_string())?;
    if !out.status.success() {
        return Err("not a git repository".into());
    }
    let top = std::fs::canonicalize(String::from_utf8_lossy(&out.stdout).trim()).map_err(|e| e.to_string())?;
    let prefix = root
        .strip_prefix(&top)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let prefix = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };

    let mut child = Command::new("git")
        .arg("-C").arg(&top)
        .args(["-c", "core.quotepath=off", "log", "--no-renames", "--name-only", "--format=%x00%ct"])
        .arg(format!("--max-count={max_commits}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut files: HashMap<String, (i64, u32)> = HashMap::new();
    let mut ts = 0i64;
    let mut commits = 0;
    for line in BufReader::new(child.stdout.take().unwrap()).split(b'\n') {
        let Ok(line) = line else { break };
        if line.is_empty() {
            continue;
        }
        if line[0] == 0 {
            ts = String::from_utf8_lossy(&line[1..]).trim().parse().unwrap_or(0);
            commits += 1;
            continue;
        }
        let p = String::from_utf8_lossy(&line);
        let Some(rel) = p.strip_prefix(prefix.as_str()) else { continue };
        let e = files.entry(rel.to_string()).or_insert((ts, 0));
        e.0 = e.0.max(ts);
        e.1 += 1;
    }
    let _ = child.wait();
    Ok(GitStats { files, commits, truncated: commits >= max_commits })
}

/// unix 時刻 → "YYYY-MM-DD" (UTC)
pub fn ymd(ts: i64) -> String {
    let z = ts.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}")
}
