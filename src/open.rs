//! ファイルを外部アプリで開く

use std::path::Path;
use std::process::{Command, Stdio};

fn spawn(cmd: &mut Command) -> Result<(), String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{:?}: {e}", cmd.get_program()))
}

pub fn in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
        .unwrap_or(false)
}

/// 既定のアプリで開く
pub fn open_default(p: &Path) -> Result<(), String> {
    spawn(Command::new("xdg-open").arg(p))
}

/// ファイルマネージャで場所を開く（対応していれば選択状態）
pub fn reveal(p: &Path) -> Result<(), String> {
    let uri = format!("file://{}", p.display());
    let ok = Command::new("dbus-send")
        .args([
            "--session", "--print-reply", "--dest=org.freedesktop.FileManager1",
            "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1.ShowItems",
            &format!("array:string:{uri}"), "string:",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        return Ok(());
    }
    open_default(p.parent().unwrap_or(p))
}

const TERMINAL_EDITORS: &[&str] = &["vim", "nvim", "vi", "nano", "micro", "hx", "helix", "kak", "ne", "joe", "mcedit", "emacs -nw"];

/// $TILESCOPE_EDITOR → $VISUAL → $EDITOR で開く。line は 1 始まり
pub fn open_in_editor(p: &Path, line: Option<usize>) -> Result<(), String> {
    let ed = ["TILESCOPE_EDITOR", "VISUAL", "EDITOR"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|s| !s.trim().is_empty()));
    let Some(ed) = ed else { return open_default(p) };
    let mut parts: Vec<String> = ed.split_whitespace().map(String::from).collect();
    let prog = parts.remove(0);
    let base = Path::new(&prog).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let path = p.display().to_string();
    let mut args = parts.clone();
    match (base.as_str(), line) {
        ("code" | "codium" | "code-insiders" | "cursor" | "windsurf", Some(l)) => {
            args.extend(["-g".into(), format!("{path}:{l}")])
        }
        ("subl" | "sublime_text" | "zed" | "hx" | "helix", Some(l)) => args.push(format!("{path}:{l}")),
        ("kate", Some(l)) => args.extend(["-l".into(), l.to_string(), path]),
        ("vim" | "nvim" | "vi" | "gvim" | "nano" | "micro" | "emacs" | "emacsclient" | "kak" | "gedit" | "ne" | "joe", Some(l)) => {
            args.extend([format!("+{l}"), path])
        }
        _ => args.push(path),
    }
    let is_term = TERMINAL_EDITORS.iter().any(|t| {
        let mut w = t.split_whitespace();
        w.next() == Some(base.as_str()) && w.all(|f| parts.iter().any(|p| p == f))
    }) || matches!(base.as_str(), "vim" | "nvim" | "vi" | "nano" | "micro" | "hx" | "helix" | "kak");
    if !is_term {
        return spawn(Command::new(&prog).args(&args));
    }
    // 端末エディタは端末エミュレータ経由で起動
    let terms: Vec<String> = std::env::var("TERMINAL").ok().into_iter()
        .chain(["x-terminal-emulator", "gnome-terminal", "konsole", "kitty", "alacritty", "wezterm",
                "foot", "xfce4-terminal", "tilix", "xterm"].iter().map(|s| s.to_string()))
        .collect();
    let term = terms.into_iter().find(|t| in_path(t)).ok_or("terminal emulator not found")?;
    let mut cmd = Command::new(&term);
    match term.as_str() {
        "gnome-terminal" => cmd.arg("--"),
        "wezterm" => cmd.args(["start", "--"]),
        "kitty" | "foot" => &mut cmd,
        _ => cmd.arg("-e"),
    };
    spawn(cmd.arg(&prog).args(&args))
}
