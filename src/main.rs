//! tilescope — WizTree風 treemap + ズームでコード表示 (Rust / egui / wgpu)
//!
//! usage: tilescope [DIR] [--metric bytes|lines|equal] [--lines] [--equal] [--ext py,rs] [--exclude PAT]... [--hidden]
//!                [--no-default-excludes] [--max-kb N]

mod git;
mod open;
mod search;

use eframe::egui::{self, Align2, Color32, FontId, Key, Mesh, Pos2, Rect, Sense, Vec2};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

const NONE: u32 = u32::MAX;
const DEFAULT_EXCLUDES: &[&str] = &[
    ".git", ".hg", ".svn", "node_modules", "__pycache__", ".venv", "venv", ".mypy_cache",
    ".pytest_cache", ".tox", ".idea", ".vscode", ".cache", ".next", ".gradle", "target", "dist",
    "build",
];
const CACHE_LIMIT: usize = 512 << 20;
const IMG_LIMIT: usize = 384 << 20;

// ---------------------------------------------------------------- options

#[derive(Clone, Copy, PartialEq)]
enum Metric {
    Bytes,
    Lines,
    Equal,
}

impl Metric {
    const ALL: [(Metric, &'static str); 3] =
        [(Metric::Bytes, "bytes"), (Metric::Lines, "lines"), (Metric::Equal, "equal")];
    fn name(self) -> &'static str {
        Self::ALL.iter().find(|m| m.0 == self).unwrap().1
    }
}

struct Opts {
    root: PathBuf,
    metric: Metric,
    exts: Option<HashSet<String>>,
    excludes: Vec<String>,
    hidden: bool,
    max_bytes: u64,
    max_img_bytes: u64,
    git_max_commits: usize,
    color: ColorMode,
    grep: Option<String>,
}

fn parse_args() -> Opts {
    let mut o = Opts {
        root: PathBuf::from("."),
        metric: Metric::Bytes,
        exts: None,
        excludes: vec![],
        hidden: false,
        max_bytes: 1024 * 1024,
        max_img_bytes: 64 << 20,
        git_max_commits: 50_000,
        color: ColorMode::Ext,
        grep: None,
    };
    let mut defaults = true;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!(
                    "usage: tilescope [DIR] [--metric bytes|lines|equal] [--lines] [--equal] [--ext py,rs] [--exclude PAT]... [--hidden] \
                     [--no-default-excludes] [--max-kb N] [--max-image-mb N] [--git-max-commits N]\n               [--color ext|age|churn] [--grep TEXT]\n\n\
                     wheel: zoom  drag: pan  double-click: fit  right-click: menu  o: open in editor\n\
                     click folder bar: show only that folder  Backspace: up  Alt+Left/Right: back/forward\n\
                     Esc: zoom out  0/Home: fit  m: minimap  n/Enter: next search hit"
                );
                std::process::exit(0);
            }
            "--lines" => o.metric = Metric::Lines,
            "--equal" => o.metric = Metric::Equal,
            "--metric" => {
                let v = it.next().unwrap_or_default();
                o.metric = Metric::ALL.iter().find(|m| m.1 == v).map(|m| m.0).unwrap_or_else(|| {
                    eprintln!("--metric: bytes | lines | equal");
                    std::process::exit(2)
                });
            }
            "--hidden" => o.hidden = true,
            "--no-default-excludes" => defaults = false,
            "--ext" => {
                o.exts = it.next().map(|s| {
                    s.split(',').map(|x| x.trim().trim_start_matches('.').to_lowercase()).collect()
                })
            }
            "--exclude" => o.excludes.extend(it.next()),
            "--color" => {
                o.color = match it.next().as_deref() {
                    Some("age") => ColorMode::Age,
                    Some("churn") => ColorMode::Churn,
                    Some("ext") => ColorMode::Ext,
                    _ => {
                        eprintln!("--color: ext | age | churn");
                        std::process::exit(2)
                    }
                }
            }
            "--grep" => o.grep = it.next(),
            "--git-max-commits" => {
                o.git_max_commits = it.next().and_then(|s| s.parse().ok()).unwrap_or(50_000)
            }
            "--max-image-mb" => {
                o.max_img_bytes = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(64) << 20
            }
            "--max-kb" => {
                o.max_bytes = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(1024) * 1024
            }
            _ if a.starts_with('-') => {
                eprintln!("unknown option: {a}");
                std::process::exit(2);
            }
            _ => o.root = PathBuf::from(a),
        }
    }
    if defaults {
        o.excludes.extend(DEFAULT_EXCLUDES.iter().map(|s| s.to_string()));
    }
    o.root = std::fs::canonicalize(&o.root).unwrap_or_else(|e| {
        eprintln!("{}: {e}", o.root.display());
        std::process::exit(1)
    });
    o
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => glob(&p[1..], s) || (!s.is_empty() && glob(p, &s[1..])),
        (Some(b'?'), Some(_)) => glob(&p[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => glob(&p[1..], &s[1..]),
        _ => false,
    }
}

fn ext_of(name: &str) -> String {
    let l = name.to_lowercase();
    match l.rfind('.') {
        Some(i) if i > 0 => l[i + 1..].to_string(),
        _ => match l.as_str() {
            "makefile" | "gnumakefile" => "mk".into(),
            "dockerfile" => "dockerfile".into(),
            _ => "(none)".into(),
        },
    }
}

// ---------------------------------------------------------------- tree

struct Node {
    name: String,
    parent: u32,
    children: Vec<u32>,
    is_dir: bool,
    size: u64,
    lines: u32,
    ext: u16,
    val: f64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    hdr: f64,
    dom: Color32,
    git_last: i64,
    git_count: u32,
}

impl Node {
    fn new(name: String, parent: u32, is_dir: bool, size: u64, ext: u16) -> Self {
        Node {
            name, parent, children: vec![], is_dir, size, lines: 0, ext, val: 0.0,
            x: 0.0, y: 0.0, w: 0.0, h: 0.0, hdr: 0.0, dom: Color32::GRAY, git_last: 0, git_count: 0,
        }
    }
}

struct Tree {
    nodes: Vec<Node>,
    ext_names: Vec<String>,
    ext_colors: Vec<Color32>,
    ext_kinds: Vec<Kind>,
}

fn scan(t: &mut Tree, ext_ids: &mut HashMap<String, u16>, idx: u32, path: &Path, o: &Opts) {
    let Ok(rd) = std::fs::read_dir(path) else { return };
    let mut subdirs = vec![];
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if (!o.hidden && name.starts_with('.'))
            || o.excludes.iter().any(|p| glob(p.as_bytes(), name.as_bytes()))
        {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let id = t.nodes.len() as u32;
        if ft.is_dir() {
            t.nodes.push(Node::new(name, idx, true, 0, 0));
            subdirs.push((id, e.path()));
        } else if ft.is_file() {
            let ext = ext_of(&name);
            if let Some(f) = &o.exts {
                if !f.contains(&ext) {
                    continue;
                }
            }
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            let n = ext_ids.len() as u16;
            let eid = *ext_ids.entry(ext.clone()).or_insert_with(|| {
                t.ext_names.push(ext.clone());
                t.ext_colors.push(color_of(&ext));
                t.ext_kinds.push(kind_of(&ext));
                n
            });
            t.nodes.push(Node::new(name, idx, false, size, eid));
        } else {
            continue;
        }
        t.nodes[idx as usize].children.push(id);
    }
    for (id, p) in subdirs {
        scan(t, ext_ids, id, &p, o);
    }
}

impl Tree {
    fn path(&self, root: &Path, mut i: u32) -> PathBuf {
        let mut parts = vec![];
        while self.nodes[i as usize].parent != NONE {
            parts.push(&self.nodes[i as usize].name);
            i = self.nodes[i as usize].parent;
        }
        let mut p = root.to_path_buf();
        for s in parts.iter().rev() {
            p.push(s);
        }
        p
    }

    fn compute_val(&mut self, i: usize, metric: Metric) -> f64 {
        if !self.nodes[i].is_dir {
            let n = &mut self.nodes[i];
            n.val = match metric {
                Metric::Bytes => n.size.max(1) as f64,
                Metric::Lines => n.lines.max(1) as f64,
                Metric::Equal => 1.0,
            };
            return n.val;
        }
        let kids = std::mem::take(&mut self.nodes[i].children);
        let mut sum = 0.0;
        for &k in &kids {
            sum += self.compute_val(k as usize, metric);
        }
        let mut kids = kids;
        kids.retain(|&k| !(self.nodes[k as usize].is_dir && self.nodes[k as usize].children.is_empty()));
        kids.sort_by(|a, b| self.nodes[*b as usize].val.total_cmp(&self.nodes[*a as usize].val));
        let dom = kids.first().map(|&k| {
            let n = &self.nodes[k as usize];
            if n.is_dir { n.dom } else { self.ext_colors[n.ext as usize] }
        });
        let n = &mut self.nodes[i];
        n.children = kids;
        n.val = sum.max(1.0);
        n.dom = dom.unwrap_or(Color32::GRAY);
        n.val
    }

    fn zero(&mut self, i: usize, x: f64, y: f64) {
        let n = &mut self.nodes[i];
        (n.x, n.y, n.w, n.h) = (x, y, 0.0, 0.0);
        for k in self.nodes[i].children.clone() {
            self.zero(k as usize, x, y);
        }
    }

    fn layout(&mut self, i: usize, x: f64, y: f64, w: f64, h: f64) {
        let n = &mut self.nodes[i];
        (n.x, n.y, n.w, n.h) = (x, y, w, h);
        if !n.is_dir || n.children.is_empty() {
            return;
        }
        let m = w.min(h);
        let pad = m * 0.008;
        let hdr = (h * 0.2).min(m * 0.05);
        n.hdr = hdr;
        let kids = n.children.clone();
        let (iw, ih) = (w - 2.0 * pad, h - hdr - pad);
        if iw <= 0.0 || ih <= 0.0 {
            for k in kids {
                self.zero(k as usize, x, y);
            }
            return;
        }
        self.squarify(&kids, x + pad, y + hdr, iw, ih);
    }

    fn squarify(&mut self, k: &[u32], mut x: f64, mut y: f64, mut w: f64, mut h: f64) {
        let v = |s: &Self, j: usize| s.nodes[k[j] as usize].val;
        let tot: f64 = (0..k.len()).map(|j| v(self, j)).sum();
        let sc = w * h / tot;
        let mut i = 0;
        while i < k.len() {
            let sh2 = w.min(h).powi(2);
            let mx = v(self, i) * sc;
            let (mut j, mut sum, mut worst) = (i, 0.0, f64::INFINITY);
            while j < k.len() {
                let a = v(self, j) * sc;
                let ns = sum + a;
                let s2 = ns * ns;
                let wr = (sh2 * mx / s2).max(s2 / (sh2 * a));
                if j > i && wr > worst {
                    break;
                }
                worst = wr;
                sum = ns;
                j += 1;
            }
            if w >= h {
                let cw = sum / h;
                let mut yy = y;
                for q in i..j {
                    let kh = v(self, q) * sc / cw;
                    self.layout(k[q] as usize, x, yy, cw, kh);
                    yy += kh;
                }
                x += cw;
                w -= cw;
            } else {
                let rh = sum / w;
                let mut xx = x;
                for q in i..j {
                    let kw = v(self, q) * sc / rh;
                    self.layout(k[q] as usize, xx, y, kw, rh);
                    xx += kw;
                }
                y += rh;
                h -= rh;
            }
            i = j;
        }
    }

    fn hit(&self, i: usize, wx: f64, wy: f64) -> Option<u32> {
        let n = &self.nodes[i];
        if n.w <= 0.0 || wx < n.x || wy < n.y || wx > n.x + n.w || wy > n.y + n.h {
            return None;
        }
        for &k in &n.children {
            if let Some(r) = self.hit(k as usize, wx, wy) {
                return Some(r);
            }
        }
        Some(i as u32)
    }
}

// ---------------------------------------------------------------- colors

fn hex(s: &str) -> Color32 {
    let v = u32::from_str_radix(s, 16).unwrap_or(0x888888);
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn hsl(h: f32, s: f32, l: f32) -> Color32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0), 1 => (x, c, 0.0), 2 => (0.0, c, x),
        3 => (0.0, x, c), 4 => (x, 0.0, c), _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let f = |v: f32| ((v + m) * 255.0) as u8;
    Color32::from_rgb(f(r), f(g), f(b))
}

fn color_of(ext: &str) -> Color32 {
    let k = match ext {
        "py" | "pyi" => "3572A5", "js" | "mjs" | "cjs" => "e8d44d", "ts" | "tsx" | "mts" => "3178c6",
        "jsx" => "61dafb", "c" => "6e8fbf", "h" => "8fa8d0", "cc" | "cpp" | "cxx" => "f34b7d",
        "hpp" | "hh" | "hxx" => "f37ba0", "rs" => "dea584", "go" => "00ADD8", "java" => "b07219",
        "kt" | "kts" => "A97BFF", "rb" => "d13b3b", "php" => "4F5D95", "cs" => "178600",
        "swift" => "F05138", "sh" | "bash" | "zsh" | "fish" => "89e051", "html" | "htm" => "e34c26",
        "css" => "663399", "scss" | "sass" => "c6538c", "json" => "b5a642", "yaml" | "yml" => "cb171e",
        "toml" => "9c4221", "md" | "rst" => "4a7fbf", "txt" => "888888", "lua" => "2C2D72",
        "vue" => "41b883", "svelte" => "ff3e00", "sql" => "e38c00", "xml" => "0060ac",
        "zig" => "ec915c", "hs" => "5e5086", "ex" | "exs" => "6e4a7e", "scala" => "c22d40",
        "dart" => "00B4AB", "r" => "198CE7", "jl" => "a270ba", "cmake" => "DA3434", "mk" => "427819",
        "dockerfile" => "384d54", "pdf" => "d93a3a", "epub" => "5b9e4d", "svg" => "ffb13b",
        "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" | "wmv" | "flv" | "mpg" | "mpeg" | "3gp" | "ogv" => "8e5bd6", "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff" => "c08a2e", "proto" => "60a0b0", "(none)" => "777777",
        _ => {
            let h = ext.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
            return hsl((h % 360) as f32, 0.5, 0.52);
        }
    };
    hex(k)
}

// ---------------------------------------------------------------- file text loading

struct FileText {
    lines: Vec<String>,
    ind: Vec<u16>,
    len: Vec<u16>,
    maxlen: usize,
    binary: bool,
    bytes: usize,
    last: u64,
    md: Option<Vec<u8>>,
}

fn load_text(path: &Path, max: u64) -> Option<FileText> {
    let data = std::fs::read(path).ok()?;
    if data.len() as u64 > max {
        return None;
    }
    let mut ft = FileText {
        lines: vec![], ind: vec![], len: vec![], maxlen: 0, binary: false,
        bytes: data.len() + 64, last: 0, md: None,
    };
    if data[..data.len().min(8192)].contains(&0) {
        ft.binary = true;
        ft.bytes = 64;
        return Some(ft);
    }
    let text = String::from_utf8_lossy(&data);
    for l in text.split('\n') {
        let l = l.strip_suffix('\r').unwrap_or(l);
        let mut s: String = l.replace('\t', "    ");
        if s.len() > 400 {
            let cut = s.char_indices().nth(400).map(|(i, _)| i).unwrap_or(s.len());
            s.truncate(cut);
        }
        let n = s.chars().count();
        let ind = s.chars().take_while(|c| *c == ' ').count();
        ft.maxlen = ft.maxlen.max(n);
        ft.ind.push(ind as u16);
        ft.len.push(n as u16);
        ft.lines.push(s);
    }
    if ft.lines.len() > 1 && ft.lines.last().is_some_and(|l| l.is_empty()) {
        ft.lines.pop();
        ft.ind.pop();
        ft.len.pop();
    }
    ft.maxlen = ft.maxlen.min(160);
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if matches!(ext.as_str(), "md" | "markdown" | "mdown" | "mkd") {
        ft.md = Some(md_kinds(&ft.lines));
    }
    Some(ft)
}

// ---- Markdown（行単位の簡易整形）

const MD_CODE: u8 = 7;
const MD_QUOTE: u8 = 8;
const MD_LIST: u8 = 9;
const MD_HR: u8 = 10;
const MD_TABLE: u8 = 11;

fn md_kinds(lines: &[String]) -> Vec<u8> {
    let mut in_code = false;
    lines
        .iter()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("```") || t.starts_with("~~~") {
                in_code = !in_code;
                return MD_CODE;
            }
            if in_code {
                return MD_CODE;
            }
            let h = t.bytes().take_while(|b| *b == b'#').count();
            if (1..=6).contains(&h) && (t.len() == h || t.as_bytes()[h] == b' ') {
                return h as u8;
            }
            if t.starts_with('>') {
                return MD_QUOTE;
            }
            let tt = t.trim_end();
            if tt.len() >= 3 && (tt.chars().all(|c| c == '-') || tt.chars().all(|c| c == '*') || tt.chars().all(|c| c == '_')) {
                return MD_HR;
            }
            let d = t.bytes().take_while(|b| b.is_ascii_digit()).count();
            if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") || (d > 0 && t[d..].starts_with(". ")) {
                return MD_LIST;
            }
            if t.starts_with('|') {
                return MD_TABLE;
            }
            0
        })
        .collect()
}

/// **強調** / `code` / [リンク](url) / ![画像](url) の記号を取り除く
fn md_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < c.len() {
        let img = c[i] == '!' && c.get(i + 1) == Some(&'[');
        if c[i] == '[' || img {
            let st = if img { i + 2 } else { i + 1 };
            if let Some(cl) = (st..c.len()).find(|&j| c[j] == ']') {
                if c.get(cl + 1) == Some(&'(') {
                    if let Some(pe) = (cl + 2..c.len()).find(|&j| c[j] == ')') {
                        let txt: String = c[st..cl].iter().collect();
                        out.push_str(&if img { format!("[{txt}]") } else { txt });
                        i = pe + 1;
                        continue;
                    }
                }
            }
        }
        if (c[i] == '*' || c[i] == '_') && c.get(i + 1) == Some(&c[i]) {
            i += 2;
            continue;
        }
        if c[i] == '`' {
            i += 1;
            continue;
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

#[derive(Default)]
struct CodeLayout {
    tx: f64,
    ty: f64,
    th: f64,
    cols: usize,
    per: usize,
    col_w: f64,
    lh: f64,
    hdr: f64,
}

/// コード表示の段組み（全てタイル寸法に比例＝ズームしても不変）
fn code_layout(x: f64, y: f64, w: f64, h: f64, f: &FileText) -> CodeLayout {
    let hdr = w.min(h) * 0.04;
    let m = w.min(h) * 0.012;
    let (tx, ty, tw, th) = (x + m + 2.0, y + hdr + m, w - 2.0 * m - 2.0, h - hdr - 2.0 * m);
    let nl = f.lines.len().max(1);
    let cw = f.maxlen.max(40) as f64 * 0.5 + 3.0;
    let (mut best, mut cols) = (0.0, 1);
    for cc in 1..=nl.min(12) {
        let v = (th / nl.div_ceil(cc) as f64).min(tw / cc as f64 / cw);
        if v > best {
            best = v;
            cols = cc;
        }
    }
    let per = nl.div_ceil(cols);
    let col_w = tw / cols as f64;
    let lh = if w > 12.0 && h > 12.0 { (th / per as f64).min(col_w / cw).max(0.0) } else { 0.0 };
    CodeLayout { tx, ty, th, cols, per, col_w, lh, hdr }
}

// ---- 色

const HIT: Color32 = Color32::from_rgb(0xff, 0xd8, 0x00);
const HIT_BG: Color32 = Color32::from_rgba_premultiplied(0x50, 0x44, 0x00, 0x90);
const NOGIT: Color32 = Color32::from_rgb(0x4a, 0x4a, 0x4e);

#[derive(Clone, Copy, PartialEq)]
enum ColorMode {
    Ext,
    Age,
    Churn,
}

impl ColorMode {
    const ALL: [(ColorMode, &'static str); 3] =
        [(ColorMode::Ext, "extension"), (ColorMode::Age, "git last modified"), (ColorMode::Churn, "git churn")];
    fn name(self) -> &'static str {
        Self::ALL.iter().find(|m| m.0 == self).unwrap().1
    }
}

/// 0 = 冷 (古い/少ない) → 1 = 熱 (新しい/多い)
fn ramp(t: f32) -> Color32 {
    const S: [(f32, [f32; 3]); 5] = [
        (0.0, [0x24 as f32, 0x30 as f32, 0x5e as f32]),
        (0.3, [0x2f as f32, 0x6f as f32, 0xb4 as f32]),
        (0.55, [0x3f as f32, 0xa8 as f32, 0x8a as f32]),
        (0.8, [0xf0 as f32, 0xc0 as f32, 0x40 as f32]),
        (1.0, [0xff as f32, 0x4a as f32, 0x36 as f32]),
    ];
    let t = t.clamp(0.0, 1.0);
    let j = S.iter().position(|s| s.0 >= t).unwrap_or(4).max(1);
    let (a, b) = (S[j - 1], S[j]);
    let u = (t - a.0) / (b.0 - a.0);
    let m = |k: usize| (a.1[k] + (b.1[k] - a.1[k]) * u) as u8;
    Color32::from_rgb(m(0), m(1), m(2))
}

fn count_lines(path: &Path, size: u64, max: u64) -> u32 {
    if size > max {
        return (size / 40) as u32; // 巨大ファイルは推定
    }
    std::fs::read(path)
        .map(|d| {
            if d[..d.len().min(8192)].contains(&0) {
                0
            } else {
                let n = d.iter().filter(|&&b| b == b'\n').count();
                (n + (!d.is_empty() && *d.last().unwrap() != b'\n') as usize) as u32
            }
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------- images / pdf

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Image,
    Pdf,
    Epub,
    Video,
    Svg,
}

fn kind_of(ext: &str) -> Kind {
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff" => Kind::Image,
        "pdf" => Kind::Pdf,
        "epub" => Kind::Epub,
        "svg" => Kind::Svg,
        "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" | "wmv" | "flv" | "mpg" | "mpeg" | "3gp" | "ogv" => Kind::Video,
        _ => Kind::Text,
    }
}

/// サムネイル解像度の段階（長辺px）
const LEVELS: [u32; 3] = [256, 1024, 2048];

fn level_for(screen: f64) -> usize {
    LEVELS.iter().position(|&l| screen <= l as f64 * 1.2).unwrap_or(LEVELS.len() - 1)
}

struct ImgEntry {
    tex: Option<egui::TextureHandle>, // None = 読めなかった
    level: usize,
    src: (u32, u32),
    note: String,
    bytes: usize,
    last: u64,
}

struct Job {
    id: u32,
    path: PathBuf,
    kind: Kind,
    level: usize,
}

enum Loaded {
    Text(u32, Option<FileText>),
    Image(u32, usize, Option<(egui::ColorImage, (u32, u32), String)>),
}

/// 毎フレーム作り直す要求キュー。ワーカーは末尾（＝優先度最大）から取る
#[derive(Default)]
struct Shared {
    queue: Vec<Job>,
    inflight: HashSet<u32>,
}

type SharedQ = Arc<(Mutex<Shared>, std::sync::Condvar)>;

fn thumb_cache_path(dir: &Path, path: &Path, level: u32) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let m = std::fs::metadata(path).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    m.len().hash(&mut h);
    m.modified().ok()?.hash(&mut h);
    level.hash(&mut h);
    Some(dir.join(format!("{:016x}.png", h.finish())))
}

// ---- EPUB: OPF を読んで表紙画像を探す（cover-image → meta cover → "cover" を含む画像 → 最初の画像）

fn xml_tags(xml: &str, name: &str) -> Vec<HashMap<String, String>> {
    let mut out = vec![];
    let mut rest = xml;
    while let Some(i) = rest.find('<') {
        rest = &rest[i + 1..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];
        rest = &rest[end..];
        let tname = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        let local = tname.rsplit(':').next().unwrap_or("");
        if !local.eq_ignore_ascii_case(name) {
            continue;
        }
        let mut attrs = HashMap::new();
        let mut a = &tag[tname.len()..];
        while let Some(eq) = a.find('=') {
            let key = a[..eq].trim().rsplit(|c: char| c.is_whitespace()).next().unwrap_or("").to_lowercase();
            let v = a[eq + 1..].trim_start();
            let Some(q) = v.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
            let v = &v[1..];
            let Some(close) = v.find(q) else { break };
            attrs.insert(key, v[..close].to_string());
            a = &v[close + 1..];
        }
        out.push(attrs);
    }
    out
}

fn zip_path(base: &str, href: &str) -> String {
    let href = href.split('#').next().unwrap_or("");
    let mut dec = Vec::new();
    let b = href.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&href[i + 1..i + 3], 16) {
                dec.push(v);
                i += 3;
                continue;
            }
        }
        dec.push(b[i]);
        i += 1;
    }
    let full = format!("{base}{}", String::from_utf8_lossy(&dec));
    let mut parts: Vec<&str> = vec![];
    for p in full.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(p),
        }
    }
    parts.join("/")
}

fn epub_cover(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut z = zip::ZipArchive::new(std::io::BufReader::new(f)).ok()?;
    let mut read = |name: &str| -> Option<Vec<u8>> {
        let mut e = z.by_name(name).ok()?;
        let mut v = Vec::new();
        e.read_to_end(&mut v).ok()?;
        Some(v)
    };
    let is_img = |n: &str| {
        let l = n.to_lowercase();
        [".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp"].iter().any(|e| l.ends_with(e))
    };
    if let Some(container) = read("META-INF/container.xml") {
        let container = String::from_utf8_lossy(&container).into_owned();
        if let Some(opf_path) = xml_tags(&container, "rootfile").iter().find_map(|t| t.get("full-path").cloned()) {
            if let Some(opf) = read(&opf_path) {
                let opf = String::from_utf8_lossy(&opf).into_owned();
                let base = opf_path.rsplit_once('/').map(|(d, _)| format!("{d}/")).unwrap_or_default();
                let items = xml_tags(&opf, "item");
                let get = |t: &HashMap<String, String>, k: &str| t.get(k).cloned().unwrap_or_default();
                let image_item = |t: &&HashMap<String, String>| {
                    get(t, "media-type").starts_with("image/") || is_img(&get(t, "href"))
                };
                let cover_id = xml_tags(&opf, "meta")
                    .into_iter()
                    .find(|t| t.get("name").is_some_and(|n| n == "cover"))
                    .and_then(|t| t.get("content").cloned());
                let pick = items
                    .iter()
                    .find(|t| get(t, "properties").split_whitespace().any(|p| p == "cover-image"))
                    .or_else(|| cover_id.and_then(|id| items.iter().find(|t| get(t, "id") == id)).filter(image_item))
                    .or_else(|| {
                        items.iter().filter(image_item).find(|t| {
                            (get(t, "id") + &get(t, "href")).to_lowercase().contains("cover")
                        })
                    })
                    .or_else(|| items.iter().find(image_item));
                if let Some(t) = pick {
                    if let Some(b) = read(&zip_path(&base, &get(t, "href"))) {
                        return Some(b);
                    }
                }
            }
        }
    }
    // OPF が壊れている場合: ZIP 内の最初の画像
    let name = (0..z.len()).filter_map(|i| z.name_for_index(i)?.ok().map(|s| s.into_owned())).find(|n| is_img(n.as_str()))?;
    let mut e = z.by_name(&name).ok()?;
    let mut v = Vec::new();
    e.read_to_end(&mut v).ok()?;
    Some(v)
}

fn to_color_image(img: image::DynamicImage) -> egui::ColorImage {
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw())
}

fn load_image(job: &Job, cache_dir: &Option<PathBuf>, max_img: u64) -> Option<(egui::ColorImage, (u32, u32), String)> {
    let lv = LEVELS[job.level];
    let cached = cache_dir.as_ref().and_then(|d| thumb_cache_path(d, &job.path, lv));
    // ディスクキャッシュ（PDF/EPUB/動画/SVG と大きい画像の縮小版）
    if let Some(c) = &cached {
        if let Ok(img) = image::open(c) {
            let meta = std::fs::read_to_string(c.with_extension("dim")).unwrap_or_default();
            let mut it = meta.splitn(2, '\n');
            let src = it.next()
                .and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?))))
                .unwrap_or((img.width(), img.height()));
            return Some((to_color_image(img), src, it.next().unwrap_or("").to_string()));
        }
    }
    let size = std::fs::metadata(&job.path).ok()?.len();
    let fit = |img: image::DynamicImage| if img.width().max(img.height()) > lv { img.thumbnail(lv, lv) } else { img };
    let mut note = String::new();
    let (img, src) = match job.kind {
        Kind::Pdf => {
            let tmp = std::env::temp_dir().join(format!("tilescope-{}-{}", std::process::id(), job.id));
            let ok = std::process::Command::new("pdftoppm")
                .args(["-f", "1", "-l", "1", "-singlefile", "-png", "-scale-to", &lv.to_string()])
                .arg(&job.path)
                .arg(&tmp)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .ok()?
                .success();
            let png = tmp.with_extension("png");
            let img = if ok { image::open(&png).ok() } else { None };
            let _ = std::fs::remove_file(&png);
            let img = img?;
            let d = (img.width(), img.height());
            (img, d)
        }
        Kind::Epub => {
            let img = image::load_from_memory(&epub_cover(&job.path)?).ok()?;
            let src = (img.width(), img.height());
            (fit(img), src)
        }
        Kind::Video => {
            let (img, dur) = video_frame(&job.path, lv)?;
            note = dur;
            let d = (img.width(), img.height());
            (img, d)
        }
        Kind::Svg => {
            if size > max_img {
                return None;
            }
            svg_render(&job.path, lv)?
        }
        _ => {
            if size > max_img {
                return None;
            }
            let img = image::ImageReader::open(&job.path).ok()?.with_guessed_format().ok()?.decode().ok()?;
            let src = (img.width(), img.height());
            (fit(img), src)
        }
    };
    if let Some(c) = cached {
        if job.kind != Kind::Image || size > 1 << 20 {
            let _ = std::fs::create_dir_all(c.parent().unwrap());
            let _ = img.save(&c);
            let _ = std::fs::write(c.with_extension("dim"), format!("{}x{}\n{}", src.0, src.1, note));
        }
    }
    Some((to_color_image(img), src, note))
}

/// ffmpeg で代表フレームを 1 枚取り出す（3 秒地点、短い動画は先頭）。長さも返す
fn video_frame(path: &Path, lv: u32) -> Option<(image::DynamicImage, String)> {
    for ss in ["3", "0"] {
        let out = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-ss", ss, "-i"])
            .arg(path)
            .args(["-frames:v", "1", "-vf", &format!("scale='min({lv},iw)':-2"), "-f", "image2pipe", "-c:v", "png", "pipe:1"])
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        if out.stdout.is_empty() {
            continue;
        }
        let img = image::load_from_memory(&out.stdout).ok()?;
        let err = String::from_utf8_lossy(&out.stderr);
        let dur = err
            .split("Duration: ")
            .nth(1)
            .and_then(|s| s.split(',').next())
            .and_then(|d| {
                let v: Vec<u64> = d.split('.').next()?.split(':').filter_map(|x| x.trim().parse().ok()).collect();
                let &[h, m, s] = v.as_slice() else { return None };
                Some(if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") })
            })
            .unwrap_or_default();
        return Some((img, dur));
    }
    None
}

fn svg_render(path: &Path, lv: u32) -> Option<(image::DynamicImage, (u32, u32))> {
    use resvg::{tiny_skia, usvg};
    static FONTS: std::sync::OnceLock<Arc<usvg::fontdb::Database>> = std::sync::OnceLock::new();
    let fonts = FONTS.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    });
    let data = std::fs::read(path).ok()?;
    let opt = usvg::Options { resources_dir: path.parent().map(|p| p.to_path_buf()), fontdb: fonts.clone(), ..Default::default() };
    let tree = usvg::Tree::from_data(&data, &opt).ok()?;
    let sz = tree.size();
    let (w0, h0) = (sz.width().max(1.0), sz.height().max(1.0));
    let sc = lv as f32 / w0.max(h0);
    let (w, h) = (((w0 * sc).round() as u32).max(1), ((h0 * sc).round() as u32).max(1));
    let mut pm = tiny_skia::Pixmap::new(w, h)?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(sc, sc), &mut pm.as_mut());
    // premultiplied → straight alpha
    let mut px = pm.take();
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let img = image::RgbaImage::from_raw(w, h, px)?;
    Some((image::DynamicImage::ImageRgba8(img), (w0.round() as u32, h0.round() as u32)))
}


// ---------------------------------------------------------------- app

struct Anim {
    t0: f64,
    s0: f64,
    s1: f64,
    c0: (f64, f64),
    c1: (f64, f64),
}

struct TextOp {
    pos: Pos2,
    text: String,
    size: f32,
    mono: bool,
    color: Color32,
    clip: Rect,
    bg: Option<Color32>,
}

struct ImgOp {
    tex: egui::TextureId,
    rect: Rect,
    uv: Rect,
    tint: Color32,
}

struct DrawOut {
    bg: Mesh,
    images: Vec<ImgOp>,
    over: Vec<egui::Shape>,
    texts: Vec<TextOp>,
    req: Vec<(f64, u32, Kind, usize)>,
    touched: Vec<u32>,
    touched_img: Vec<u32>,
    budget: i64,
}

struct App {
    t: Tree,
    opts: Opts,
    metric: Metric,
    lines_counted: bool,
    ext_stats: Vec<(u16, f64, usize)>,
    total: f64,
    // view: screen = origin + world * s + (tx, ty)
    s: f64,
    tx: f64,
    ty: f64,
    anim: Option<Anim>,
    canvas: Rect,
    laid_size: Vec2,
    focus: u32,
    hover: Option<u32>,
    hl_ext: Option<u16>,
    query: String,
    qmatch: Vec<bool>,
    cache: HashMap<u32, FileText>,
    cache_bytes: usize,
    imgs: HashMap<u32, ImgEntry>,
    img_bytes: usize,
    has_pdftoppm: bool,
    has_ffmpeg: bool,
    shared: SharedQ,
    res_rx: mpsc::Receiver<Loaded>,
    frame: u64,
    ctx: egui::Context,
    // 色分け / git
    color_mode: ColorMode,
    git_rx: mpsc::Receiver<Result<git::GitStats, String>>,
    git_status: String,
    git_oldest: i64,
    git_newest: i64,
    git_max_count: u32,
    now_ts: i64,
    // 検索
    search_content: bool,
    search_gen: Arc<std::sync::atomic::AtomicU64>,
    search_rx: mpsc::Receiver<search::Batch>,
    search_tx: mpsc::Sender<search::Batch>,
    search_hits: HashMap<u32, Vec<u32>>,
    search_status: String,
    search_dirty_at: Option<f64>,
    search_files: Option<Arc<Vec<(u32, PathBuf)>>>,
    hit_order: Vec<u32>,
    hit_pos: usize,
    pending_jump: Option<(u32, usize)>,
    // 操作
    hover_line: Option<usize>,
    show_minimap: bool,
    mm_drag: bool,
    menu_target: Option<(u32, Option<usize>)>,
    status: Option<(String, f64)>,
    // 表示ルート（このディレクトリだけを画面いっぱいに表示）と履歴
    root: u32,
    back: Vec<u32>,
    fwd: Vec<u32>,
    hover_header: bool,
    root_changed_at: f64,
}

const DIRBG: [Color32; 4] = [
    Color32::from_rgb(0x2a, 0x2a, 0x2e),
    Color32::from_rgb(0x30, 0x30, 0x35),
    Color32::from_rgb(0x36, 0x36, 0x3c),
    Color32::from_rgb(0x2c, 0x2c, 0x31),
];
const HEADER_COLORS: [Color32; 8] = [
    Color32::from_rgb(0x3a, 0x5f, 0x9f),
    Color32::from_rgb(0x2f, 0x82, 0x70),
    Color32::from_rgb(0x7d, 0x55, 0xa6),
    Color32::from_rgb(0xa5, 0x66, 0x2f),
    Color32::from_rgb(0x2f, 0x73, 0x97),
    Color32::from_rgb(0x99, 0x47, 0x6a),
    Color32::from_rgb(0x5e, 0x81, 0x31),
    Color32::from_rgb(0x55, 0x5b, 0xb0),
];
const BORDER: Color32 = Color32::from_rgb(0x11, 0x11, 0x11);
const CODE_BG: Color32 = Color32::from_rgb(0x16, 0x18, 0x1d);

fn is_comment(s: &str) -> bool {
    let t = s.trim_start();
    ["#", "//", "/*", "*", "--", ";", "<!--"].iter().any(|p| t.starts_with(p))
}

fn fmt_bytes(v: f64) -> String {
    let u = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (v, 0);
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{v:.0} B") } else { format!("{v:.1} {}", u[i]) }
}

impl App {
    fn new(opts: Opts, ctx: egui::Context) -> Self {
        let t0 = std::time::Instant::now();
        let root_name = opts
            .root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| opts.root.display().to_string());
        let mut t = Tree { nodes: vec![Node::new(root_name, NONE, true, 0, 0)], ext_names: vec![], ext_colors: vec![], ext_kinds: vec![] };
        let mut ids = HashMap::new();
        scan(&mut t, &mut ids, 0, &opts.root.clone(), &opts);
        eprintln!("scanned {} entries in {:?}", t.nodes.len(), t0.elapsed());

        let (res_tx, res_rx) = mpsc::channel();
        let shared: SharedQ = Arc::new((Mutex::new(Shared::default()), std::sync::Condvar::new()));
        let max = opts.max_bytes;
        let max_img = opts.max_img_bytes;
        let cache_dir = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .map(|d| d.join("tilescope"));
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
        for _ in 0..workers {
            let sh = shared.clone();
            let tx = res_tx.clone();
            let ctx = ctx.clone();
            let cache_dir = cache_dir.clone();
            std::thread::spawn(move || loop {
                let job = {
                    let (m, cv) = &*sh;
                    let mut g = m.lock().unwrap();
                    loop {
                        if let Some(j) = g.queue.pop() {
                            g.inflight.insert(j.id);
                            break j;
                        }
                        g = cv.wait(g).unwrap();
                    }
                };
                let res = match job.kind {
                    Kind::Text => Loaded::Text(job.id, load_text(&job.path, max)),
                    _ => Loaded::Image(job.id, job.level, load_image(&job, &cache_dir, max_img)),
                };
                if tx.send(res).is_err() {
                    break;
                }
                sh.0.lock().unwrap().inflight.remove(&job.id);
                ctx.request_repaint();
            });
        }
        let has_tool = |cmd: &str, arg: &str| std::process::Command::new(cmd).arg(arg)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().is_ok();
        let has_pdftoppm = has_tool("pdftoppm", "-v");
        let has_ffmpeg = has_tool("ffmpeg", "-version");

        let (git_tx, git_rx) = mpsc::channel();
        {
            let root = opts.root.clone();
            let ctx = ctx.clone();
            let max = opts.git_max_commits;
            std::thread::spawn(move || {
                let _ = git_tx.send(git::load(&root, max));
                ctx.request_repaint();
            });
        }
        let (search_tx, search_rx) = mpsc::channel();
        let now_ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);

        let metric = opts.metric;
        let (color0, grep0) = (opts.color, opts.grep.clone());
        let mut app = App {
            t, opts, metric, lines_counted: false, ext_stats: vec![], total: 0.0,
            s: 1.0, tx: 0.0, ty: 0.0, anim: None, canvas: Rect::NOTHING, laid_size: Vec2::ZERO,
            focus: 0, hover: None, hl_ext: None, query: grep0.clone().unwrap_or_default(), qmatch: vec![],
            cache: HashMap::new(), cache_bytes: 0, imgs: HashMap::new(), img_bytes: 0, has_pdftoppm,
            has_ffmpeg, shared, res_rx, frame: 0, ctx,
            color_mode: color0, git_rx, git_status: "git: loading…".into(), git_oldest: 0, git_newest: 0,
            git_max_count: 0, now_ts,
            search_content: grep0.is_some(), search_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)), search_rx, search_tx,
            search_hits: HashMap::new(), search_status: String::new(), search_dirty_at: grep0.as_ref().map(|_| -1.0), search_files: None,
            hit_order: vec![], hit_pos: 0, pending_jump: None,
            hover_line: None, show_minimap: true, mm_drag: false, menu_target: None, status: None,
            root: 0, back: vec![], fwd: vec![], hover_header: false, root_changed_at: -10.0,
        };
        app.recompute_values();
        app
    }

    fn ensure_lines(&mut self) {
        if self.lines_counted {
            return;
        }
        let max = self.opts.max_bytes * 8;
        let files: Vec<(usize, PathBuf, u64)> = (0..self.t.nodes.len())
            .filter(|&i| !self.t.nodes[i].is_dir)
            .map(|i| (i, self.t.path(&self.opts.root, i as u32), self.t.nodes[i].size))
            .collect();
        let counts: Vec<(usize, u32)> =
            files.par_iter().map(|(i, p, s)| (*i, count_lines(p, *s, max))).collect();
        for (i, c) in counts {
            self.t.nodes[i].lines = c;
        }
        self.lines_counted = true;
    }

    fn recompute_values(&mut self) {
        if self.metric == Metric::Lines {
            self.ensure_lines();
        }
        self.t.compute_val(0, self.metric);
        self.compute_ext_stats();
        self.laid_size = Vec2::ZERO; // force relayout
    }

    /// 表示ルート配下の拡張子別集計
    fn compute_ext_stats(&mut self) {
        let mut m: HashMap<u16, (f64, usize)> = HashMap::new();
        let mut total = 0.0;
        let mut stack = vec![self.root as usize];
        while let Some(i) = stack.pop() {
            let n = &self.t.nodes[i];
            if n.is_dir {
                stack.extend(n.children.iter().map(|&k| k as usize));
            } else {
                let e = m.entry(n.ext).or_default();
                e.0 += n.val;
                e.1 += 1;
                total += n.val;
            }
        }
        let mut v: Vec<_> = m.into_iter().map(|(k, (a, b))| (k, a, b)).collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        self.ext_stats = v;
        self.total = total;
    }

    fn depth(&self, mut i: u32) -> usize {
        let mut d = 0;
        while self.t.nodes[i as usize].parent != NONE {
            i = self.t.nodes[i as usize].parent;
            d += 1;
        }
        d
    }

    fn is_under(&self, mut i: u32, r: u32) -> bool {
        while i != NONE {
            if i == r {
                return true;
            }
            i = self.t.nodes[i as usize].parent;
        }
        false
    }

    /// このディレクトリだけを画面いっぱいに表示する（ファイルなら親ディレクトリ）
    fn set_root(&mut self, id: u32, now: f64, hist: bool) {
        let mut id = id;
        if !self.t.nodes[id as usize].is_dir {
            id = self.t.nodes[id as usize].parent;
        }
        if id == NONE {
            return;
        }
        if id == self.root {
            self.fit(id, now, false);
            return;
        }
        if hist {
            self.back.push(self.root);
            self.fwd.clear();
        }
        self.root = id;
        self.root_changed_at = now;
        self.compute_ext_stats();
        if self.canvas.width() > 1.0 {
            self.relayout();
            self.fit(id, now, true);
        } else {
            self.laid_size = Vec2::ZERO;
            self.focus = id;
        }
    }

    fn go_up(&mut self, now: f64) {
        let p = self.t.nodes[self.root as usize].parent;
        if p != NONE {
            let child = self.root;
            self.set_root(p, now, true);
            // 元のディレクトリから引いていくアニメーション
            let (s1, c1) = self.view_for(p);
            let (s0, c0) = self.view_for(child);
            self.set_center(s0, c0);
            self.anim = Some(Anim { t0: now, s0, s1, c0, c1 });
        }
    }

    fn go_back(&mut self, now: f64) {
        if let Some(r) = self.back.pop() {
            self.fwd.push(self.root);
            self.set_root(r, now, false);
        }
    }

    fn go_fwd(&mut self, now: f64) {
        if let Some(r) = self.fwd.pop() {
            self.back.push(self.root);
            self.set_root(r, now, false);
        }
    }

    /// 現在の表示ルートの外にあるなら、その親ディレクトリを表示ルートにする
    fn ensure_visible(&mut self, id: u32, now: f64) {
        if !self.is_under(id, self.root) {
            self.set_root(id, now, true);
        }
    }

    fn fmt_val(&self, v: f64) -> String {
        match self.metric {
            Metric::Bytes => fmt_bytes(v),
            Metric::Lines => format!("{} lines", v as u64),
            Metric::Equal => format!("{} file{}", v as u64, if v as u64 == 1 { "" } else { "s" }),
        }
    }

    fn relayout(&mut self) {
        let sz = self.canvas.size();
        let wh = 1000.0;
        let ww = wh * (sz.x as f64 / sz.y.max(1.0) as f64);
        self.t.layout(self.root as usize, 0.0, 0.0, ww, wh);
        self.laid_size = sz;
    }

    fn view_for(&self, i: u32) -> (f64, (f64, f64)) {
        let n = &self.t.nodes[i as usize];
        let (cw, ch) = (self.canvas.width() as f64, self.canvas.height() as f64);
        ((cw / n.w).min(ch / n.h) * 0.97, (n.x + n.w / 2.0, n.y + n.h / 2.0))
    }

    fn center(&self) -> (f64, (f64, f64)) {
        let (cw, ch) = (self.canvas.width() as f64, self.canvas.height() as f64);
        (self.s, ((cw / 2.0 - self.tx) / self.s, (ch / 2.0 - self.ty) / self.s))
    }

    fn set_center(&mut self, s: f64, c: (f64, f64)) {
        let (cw, ch) = (self.canvas.width() as f64, self.canvas.height() as f64);
        self.s = s;
        self.tx = cw / 2.0 - c.0 * s;
        self.ty = ch / 2.0 - c.1 * s;
    }

    fn fit(&mut self, i: u32, now: f64, instant: bool) {
        if self.t.nodes[i as usize].w <= 0.0 {
            return;
        }
        self.focus = i;
        let (s1, c1) = self.view_for(i);
        if instant {
            self.anim = None;
            self.set_center(s1, c1);
        } else {
            let (s0, c0) = self.center();
            self.anim = Some(Anim { t0: now, s0, s1, c0, c1 });
        }
    }

    fn step_anim(&mut self, now: f64) -> bool {
        let Some(a) = &self.anim else { return false };
        let mut t = ((now - a.t0) / 0.35).min(1.0);
        t = if t < 0.5 { 2.0 * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(2) / 2.0 };
        let s = (a.s0.ln() + (a.s1.ln() - a.s0.ln()) * t).exp();
        let c = (a.c0.0 + (a.c1.0 - a.c0.0) * t, a.c0.1 + (a.c1.1 - a.c0.1) * t);
        let done = t >= 1.0;
        self.set_center(s, c);
        if done {
            self.anim = None;
        }
        !done
    }

    fn update_query(&mut self) {
        let q = self.query.to_lowercase();
        if q.is_empty() {
            self.qmatch.clear();
            return;
        }
        let n = self.t.nodes.len();
        self.qmatch = vec![false; n];
        // パスは親→子の順に作れるので、部分一致はノード名を連結して判定
        let mut paths: Vec<String> = vec![String::new(); n];
        paths[0] = self.t.nodes[0].name.to_lowercase();
        let mut stack = vec![0usize];
        while let Some(i) = stack.pop() {
            for &k in &self.t.nodes[i].children {
                let k = k as usize;
                paths[k] = format!("{}/{}", paths[i], self.t.nodes[k].name.to_lowercase());
                self.qmatch[k] = paths[k].contains(&q);
                stack.push(k);
            }
        }
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), self.ctx.input(|i| i.time)));
    }

    fn apply_git(&mut self, st: git::GitStats) {
        let n = self.t.nodes.len();
        let mut rel: Vec<String> = vec![String::new(); n];
        let mut stack = vec![0usize];
        let mut tracked = 0;
        while let Some(i) = stack.pop() {
            for k in self.t.nodes[i].children.clone() {
                let k = k as usize;
                rel[k] = if i == 0 { self.t.nodes[k].name.clone() } else { format!("{}/{}", rel[i], self.t.nodes[k].name) };
                if !self.t.nodes[k].is_dir {
                    if let Some(&(ts, c)) = st.files.get(&rel[k]) {
                        self.t.nodes[k].git_last = ts;
                        self.t.nodes[k].git_count = c;
                        tracked += 1;
                    }
                }
                stack.push(k);
            }
        }
        let (mut oldest, mut newest, mut maxc) = (i64::MAX, 0, 0);
        for n in self.t.nodes.iter().filter(|n| !n.is_dir && n.git_last > 0) {
            oldest = oldest.min(n.git_last);
            newest = newest.max(n.git_last);
            maxc = maxc.max(n.git_count);
        }
        // 子は親より後ろに並んでいるので逆順で親へ集約（ディレクトリ = 最新 / 最大）
        for i in (1..n).rev() {
            let (l, c, p) = (self.t.nodes[i].git_last, self.t.nodes[i].git_count, self.t.nodes[i].parent as usize);
            let pn = &mut self.t.nodes[p];
            pn.git_last = pn.git_last.max(l);
            pn.git_count = pn.git_count.max(c);
        }
        self.git_oldest = if oldest == i64::MAX { 0 } else { oldest };
        self.git_newest = newest;
        self.git_max_count = maxc;
        self.git_status = format!(
            "git: {} commits{} / {} files",
            st.commits,
            if st.truncated { "+ (truncated)" } else { "" },
            tracked
        );
    }

    fn start_search(&mut self) {
        let generation = self.search_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        self.search_hits.clear();
        self.hit_order.clear();
        self.hit_pos = 0;
        let q = self.query.clone();
        if q.chars().count() < 2 {
            self.qmatch.clear();
            self.search_status.clear();
            return;
        }
        let files = self
            .search_files
            .get_or_insert_with(|| {
                Arc::new(
                    (0..self.t.nodes.len())
                        .filter(|&i| {
                            let n = &self.t.nodes[i];
                            !n.is_dir && n.size <= self.opts.max_bytes && self.t.ext_kinds[n.ext as usize] == Kind::Text
                        })
                        .map(|i| (i as u32, self.t.path(&self.opts.root, i as u32)))
                        .collect(),
                )
            })
            .clone();
        self.qmatch = vec![false; self.t.nodes.len()];
        self.search_status = format!("searching… 0/{}", files.len());
        let ctx = self.ctx.clone();
        search::spawn(generation, files, q, self.search_gen.clone(), self.search_tx.clone(), move || ctx.request_repaint());
    }

    fn total_hits(&self) -> usize {
        self.search_hits.values().map(|v| v.len()).sum()
    }

    /// 次の検索ヒットへ移動（内容検索なら該当行までズーム）
    fn next_hit(&mut self, now: f64) {
        if self.search_content {
            if self.hit_order.is_empty() || self.total_hits() == 0 {
                return;
            }
            // 全ヒット行を (ヒット数の多いファイル順, 行順) に巡回
            let total = self.total_hits();
            let mut k = self.hit_pos % total;
            self.hit_pos += 1;
            for &id in &self.hit_order.clone() {
                let v = &self.search_hits[&id];
                if k < v.len() {
                    let line = v[k] as usize;
                    self.jump_to_line(id, line, now);
                    self.flash(format!("hit {}/{}", self.hit_pos.min(total), total));
                    return;
                }
                k -= v.len();
            }
        } else if !self.qmatch.is_empty() {
            let mut v: Vec<usize> = (0..self.t.nodes.len()).filter(|&i| self.qmatch[i] && !self.t.nodes[i].is_dir).collect();
            v.sort_by(|&a, &b| self.t.nodes[b].val.total_cmp(&self.t.nodes[a].val));
            if !v.is_empty() {
                let id = v[self.hit_pos % v.len()] as u32;
                self.hit_pos += 1;
                self.ensure_visible(id, now);
                self.fit(id, now, false);
            }
        }
    }

    fn jump_to_line(&mut self, id: u32, line: usize, now: f64) {
        self.ensure_visible(id, now);
        let Some(f) = self.cache.get(&id).filter(|f| !f.lines.is_empty()) else {
            // 本文がまだ無い: ファイルにズームして読み込み後に再ジャンプ
            self.fit(id, now, false);
            self.pending_jump = Some((id, line));
            return;
        };
        let n = &self.t.nodes[id as usize];
        let l = code_layout(n.x, n.y, n.w, n.h, f);
        if l.lh <= 0.0 {
            return;
        }
        let (c, r) = (line / l.per, line % l.per);
        let cx = l.tx + c as f64 * l.col_w + l.col_w / 2.0;
        let cy = l.ty + (r as f64 + 0.5) * l.lh;
        // 1 行 ≒ 18px、段の幅が画面に収まる程度
        let s1 = (18.0 / l.lh).min(self.canvas.width() as f64 / l.col_w * 0.95).max(self.view_for(id).0);
        let (s0, c0) = self.center();
        self.focus = id;
        self.anim = Some(Anim { t0: now, s0, s1, c0, c1: (cx, cy) });
        self.pending_jump = None;
    }

    fn dim(&self, i: usize) -> bool {
        let n = &self.t.nodes[i];
        self.hl_ext.is_some_and(|e| e != n.ext) || (!self.qmatch.is_empty() && !self.qmatch[i])
    }

    // ------------------------------------------------------------ drawing

    fn to_screen(&self, n: &Node) -> (f64, f64, f64, f64) {
        (
            self.canvas.min.x as f64 + n.x * self.s + self.tx,
            self.canvas.min.y as f64 + n.y * self.s + self.ty,
            n.w * self.s,
            n.h * self.s,
        )
    }

    fn rect(&self, x: f64, y: f64, w: f64, h: f64) -> Rect {
        // f32 精度対策: キャンバス外は切り詰める
        let c = self.canvas.expand(4.0);
        let (x0, y0) = ((x as f32).max(c.min.x), (y as f32).max(c.min.y));
        let (x1, y1) = (((x + w) as f32).min(c.max.x), ((y + h) as f32).min(c.max.y));
        Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1.max(x0), y1.max(y0)))
    }

    fn draw_node(&self, i: usize, depth: usize, o: &mut DrawOut) {
        let n = &self.t.nodes[i];
        let (sx, sy, sw, sh) = self.to_screen(n);
        let c = self.canvas;
        if sw <= 0.0 || sh <= 0.0 || sx > c.max.x as f64 || sy > c.max.y as f64 || sx + sw < c.min.x as f64 || sy + sh < c.min.y as f64 {
            return;
        }
        if !n.is_dir {
            self.draw_file(i, sx, sy, sw, sh, o);
            return;
        }
        if sw < 5.0 || sh < 5.0 {
            o.bg.add_colored_rect(self.rect(sx, sy, sw, sh), n.dom.gamma_multiply(0.7));
            return;
        }
        o.bg.add_colored_rect(self.rect(sx, sy, sw, sh), BORDER);
        o.bg.add_colored_rect(self.rect(sx + 1.0, sy + 1.0, sw - 2.0, sh - 2.0), DIRBG[depth % 4]);
        let hh = n.hdr * self.s;
        if hh >= 2.0 {
            // ディレクトリ名のバー（階層ごとに色分け、ホバーで明るく、クリックでこのディレクトリだけを表示）
            let hot = self.hover_header && self.hover == Some(i as u32);
            let hc = HEADER_COLORS[depth % HEADER_COLORS.len()];
            let hc = if hot { hc.gamma_multiply(1.6) } else { hc };
            o.bg.add_colored_rect(self.rect(sx + 1.0, sy + 1.0, sw - 2.0, hh - 1.0), hc);
            if hot {
                o.bg.add_colored_rect(self.rect(sx + 1.0, sy + hh - 2.0, sw - 2.0, 2.0), Color32::WHITE);
            }
        }
        if hh >= 8.0 && sw > 30.0 {
            let fs = (hh * 0.72).min(16.0) as f32;
            let clip = self.rect(sx, sy, sw, hh);
            let x0 = (sx + 6.0).max(c.min.x as f64 + 4.0);
            let y0 = (sy + hh / 2.0 - fs as f64 * 0.6) as f32;
            let name = format!("{}/", n.name);
            let nw = name.chars().map(|ch| if ch.is_ascii() { 0.56 } else { 1.0 }).sum::<f64>() * fs as f64;
            o.texts.push(TextOp {
                pos: Pos2::new(x0 as f32, y0), text: name, size: fs.round(), mono: false,
                color: Color32::WHITE, clip, bg: None,
            });
            o.texts.push(TextOp {
                pos: Pos2::new((x0 + nw + fs as f64 * 0.8) as f32, y0), text: self.fmt_val(n.val), size: fs.round(), mono: false,
                color: Color32::from_white_alpha(170), clip, bg: None,
            });
        }
        for &k in &n.children {
            self.draw_node(k as usize, depth + 1, o);
        }
    }

    fn media_ok(&self, kind: Kind) -> bool {
        match kind {
            Kind::Image | Kind::Epub | Kind::Svg => true,
            Kind::Pdf => self.has_pdftoppm,
            Kind::Video => self.has_ffmpeg,
            Kind::Text => false,
        }
    }

    fn node_color(&self, i: usize) -> Color32 {
        let n = &self.t.nodes[i];
        match self.color_mode {
            ColorMode::Ext => {
                if n.is_dir { n.dom } else { self.t.ext_colors[n.ext as usize] }
            }
            ColorMode::Age => {
                if n.git_last == 0 {
                    NOGIT
                } else {
                    let span = ((self.now_ts - self.git_oldest).max(86400) as f64 / 86400.0).ln_1p();
                    let age = ((self.now_ts - n.git_last).max(0) as f64 / 86400.0).ln_1p();
                    ramp(1.0 - (age / span).clamp(0.0, 1.0) as f32)
                }
            }
            ColorMode::Churn => {
                if n.git_count == 0 {
                    NOGIT
                } else {
                    ramp(((n.git_count as f64).ln_1p() / (self.git_max_count.max(1) as f64).ln_1p()) as f32)
                }
            }
        }
    }

    /// 画面上のファイルタイル（隙間込み）
    fn file_rect(&self, i: usize) -> (f64, f64, f64, f64) {
        let (sx, sy, sw, sh) = self.to_screen(&self.t.nodes[i]);
        let g = if sw > 4.0 && sh > 4.0 { 1.0 } else { 0.0 };
        (sx + g, sy + g, sw - 2.0 * g, sh - 2.0 * g)
    }

    /// カーソル位置の行番号（0 始まり）
    fn line_at(&self, i: u32, p: Pos2) -> Option<usize> {
        let f = self.cache.get(&i).filter(|f| !f.binary && !f.lines.is_empty())?;
        let (x, y, w, h) = self.file_rect(i as usize);
        let l = code_layout(x, y, w, h, f);
        if l.lh <= 0.0 {
            return None;
        }
        let c = ((p.x as f64 - l.tx) / l.col_w).floor();
        let r = ((p.y as f64 - l.ty) / l.lh).floor();
        if c < 0.0 || r < 0.0 || c as usize >= l.cols || r as usize >= l.per {
            return None;
        }
        let li = c as usize * l.per + r as usize;
        (li < f.lines.len()).then_some(li)
    }

    fn draw_file(&self, i: usize, sx: f64, sy: f64, sw: f64, sh: f64, o: &mut DrawOut) {
        let n = &self.t.nodes[i];
        let dim = self.dim(i);
        let a = |c: Color32| if dim { c.gamma_multiply(0.13) } else { c };
        let col = self.node_color(i);
        let g = if sw > 4.0 && sh > 4.0 { 1.0 } else { 0.0 };
        let (x, y, w, h) = (sx + g, sy + g, sw - 2.0 * g, sh - 2.0 * g);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let kind = self.t.ext_kinds[n.ext as usize];
        if self.media_ok(kind) && self.draw_image(i, kind, x, y, w, h, col, dim, o) {
            return;
        }
        let ft = self.cache.get(&(i as u32));
        let small_enough = n.size <= self.opts.max_bytes;
        if ft.is_none() && small_enough && w > 24.0 && h > 24.0 {
            o.req.push((w * h, i as u32, Kind::Text, 0));
        }
        if ft.is_some() {
            o.touched.push(i as u32);
        }
        let c = self.canvas;
        let (cminx, cmaxx, cminy, cmaxy) = (c.min.x as f64, c.max.x as f64, c.min.y as f64, c.max.y as f64);
        let hits = self.search_hits.get(&(i as u32));
        let is_hit = |li: usize| hits.is_some_and(|h| h.binary_search(&(li as u32)).is_ok());

        let text = ft.filter(|f| !f.binary && !f.lines.is_empty());
        let l = text.map(|f| code_layout(x, y, w, h, f)).unwrap_or_default();
        let lh = l.lh;
        let (row_a, row_b) = if lh > 0.0 {
            (((cminy - l.ty) / lh).floor().max(0.0) as usize, (((cmaxy - l.ty) / lh).ceil().max(0.0) as usize).min(l.per))
        } else {
            (0, 0)
        };

        if lh >= 6.0 {
            // ---- 読めるコード表示
            let f = text.unwrap();
            let tile = self.rect(x, y, w, h);
            o.bg.add_colored_rect(tile, a(CODE_BG));
            o.bg.add_colored_rect(self.rect(x, y, 3.0, h), a(col));
            if l.hdr >= 8.0 {
                o.bg.add_colored_rect(self.rect(x, y, w, l.hdr), a(col));
                let fs = (l.hdr * 0.7).min(200.0) as f32;
                o.texts.push(TextOp {
                    pos: Pos2::new((x + 6.0).max(cminx) as f32, (y + l.hdr / 2.0 - fs as f64 * 0.6) as f32),
                    text: format!("{}  ({} lines, {})", n.name, f.lines.len(), fmt_bytes(n.size as f64)),
                    size: fs.round(), mono: false, color: a(Color32::BLACK),
                    clip: self.rect(x, y, w, l.hdr), bg: None,
                });
            }
            let fs = ((lh / 1.2).floor() as f32).min(256.0);
            let nd = f.lines.len().to_string().len();
            let gut = if lh >= 9.0 { nd as f64 * fs as f64 * 0.62 + 8.0 } else { 0.0 };
            for cc in 0..l.cols {
                let cx = l.tx + cc as f64 * l.col_w;
                if cx > cmaxx || cx + l.col_w < cminx {
                    continue;
                }
                let clip = self.rect(cx, l.ty, l.col_w - lh, l.th).intersect(tile);
                if cc > 0 {
                    o.bg.add_colored_rect(self.rect(cx - lh * 0.6, l.ty, 1.0, l.th), a(Color32::from_rgb(0x2a, 0x2d, 0x35)));
                }
                for r in row_a..row_b {
                    let li = cc * l.per + r;
                    if li >= f.lines.len() {
                        break;
                    }
                    let yy = l.ty + r as f64 * lh;
                    if is_hit(li) {
                        o.bg.add_colored_rect(self.rect(cx, yy, l.col_w - lh, lh).intersect(clip), a(HIT_BG));
                    }
                    if gut > 0.0 {
                        o.texts.push(TextOp {
                            pos: Pos2::new(cx as f32, yy as f32),
                            text: format!("{:>nd$}", li + 1),
                            size: fs, mono: true,
                            color: a(if is_hit(li) { HIT } else { Color32::from_gray(0x55) }),
                            clip, bg: None,
                        });
                    }
                    let lx = cx + gut;
                    let tw = l.col_w - lh - gut;
                    let raw = &f.lines[li];
                    // Markdown は行単位で整形（行グリッドは崩さない）
                    let (s, mono, size, color) = match f.md.as_ref().map(|m| m[li]) {
                        None => (std::borrow::Cow::Borrowed(raw.as_str()), true, fs,
                                 if is_comment(raw) { Color32::from_rgb(0x6a, 0x99, 0x55) } else { Color32::from_gray(0xd4) }),
                        Some(k) => {
                            let t = raw.trim_start();
                            let ind = (raw.len() - t.len()) as f64 * fs as f64 * 0.5;
                            match k {
                                MD_CODE => {
                                    o.bg.add_colored_rect(self.rect(lx, yy, tw, lh).intersect(clip), a(Color32::from_rgb(0x0e, 0x10, 0x14)));
                                    let s = if t.starts_with("```") || t.starts_with("~~~") { t.trim_start_matches(['`', '~']) } else { raw.as_str() };
                                    (s.to_string().into(), true, fs, Color32::from_rgb(0xd7, 0xba, 0x7d))
                                }
                                1..=6 => {
                                    let hc = [0x61afef, 0xc678dd, 0xe5c07b, 0x98c379, 0x56b6c2, 0xabb2bf][k as usize - 1];
                                    (md_inline(t.trim_start_matches('#').trim_start()).into(), false,
                                     (lh * 0.92).floor() as f32, Color32::from_rgb((hc >> 16) as u8, (hc >> 8) as u8, hc as u8))
                                }
                                MD_QUOTE => {
                                    o.bg.add_colored_rect(self.rect(lx + ind, yy, (lh * 0.15).max(1.0), lh), a(Color32::from_gray(0x60)));
                                    (format!("  {}", md_inline(t.trim_start_matches('>').trim_start())).into(), false, fs, Color32::from_gray(0x9a))
                                }
                                MD_HR => {
                                    o.bg.add_colored_rect(self.rect(lx, yy + lh * 0.5, tw, (lh * 0.08).max(1.0)), a(Color32::from_gray(0x50)));
                                    ("".into(), false, fs, Color32::WHITE)
                                }
                                MD_LIST => {
                                    let body = t.trim_start_matches(|c: char| matches!(c, '-' | '*' | '+')).trim_start();
                                    let pre: String = if body.len() == t.len() { String::new() } else { format!("{}• ", " ".repeat(raw.len() - t.len())) };
                                    let body = if pre.is_empty() { t } else { body };
                                    (format!("{pre}{}", md_inline(body)).into(), false, fs, Color32::from_gray(0xd4))
                                }
                                MD_TABLE => (raw.as_str().into(), true, fs, Color32::from_gray(0xc0)),
                                _ => (md_inline(raw).into(), false, fs, Color32::from_gray(0xd4)),
                            }
                        }
                    };
                    if s.trim().is_empty() {
                        continue;
                    }
                    // 左側が画面外なら文字単位で切り詰め（長い行のレイアウト負荷軽減）
                    let charw = size as f64 * 0.6;
                    let skip = if lx < cminx { ((cminx - lx) / charw) as usize } else { 0 };
                    let vis = ((cmaxx - lx.max(cminx)) / charw) as usize + 2;
                    let st: String = s.chars().skip(skip).take(vis).collect();
                    o.texts.push(TextOp {
                        pos: Pos2::new((lx + skip as f64 * charw) as f32, (yy + (lh - size as f64 * 1.2) / 2.0) as f32),
                        text: st, size, mono, color: a(color), clip, bg: None,
                    });
                }
            }
            return;
        }

        o.bg.add_colored_rect(self.rect(x, y, w, h), a(col));
        if lh >= 0.6 && o.budget > 0 {
            // ---- ミニマップ風の行シルエット（検索ヒット行は黄色）
            let f = text.unwrap();
            let cw = lh * 0.5;
            let bh = (lh * 0.6).max(0.5);
            let maxw = l.col_w - lh * 2.0;
            let shade = a(Color32::from_black_alpha(97));
            for cc in 0..l.cols {
                let cx = l.tx + cc as f64 * l.col_w;
                if cx > cmaxx || cx + l.col_w < cminx {
                    continue;
                }
                for r in row_a..row_b {
                    let li = cc * l.per + r;
                    if li >= f.lines.len() {
                        break;
                    }
                    let (ln, ix) = (f.len[li] as f64, f.ind[li] as f64);
                    if is_hit(li) {
                        o.bg.add_colored_rect(self.rect(cx, l.ty + r as f64 * lh, maxw, bh.max(1.0)), a(HIT));
                        continue;
                    }
                    if ln == 0.0 {
                        continue;
                    }
                    let bw = ((ln - ix) * cw).min(maxw - ix * cw);
                    if bw > 0.0 {
                        o.bg.add_colored_rect(self.rect(cx + ix * cw, l.ty + r as f64 * lh, bw, bh), shade);
                    }
                }
                o.budget -= (row_b - row_a) as i64;
            }
        } else if let Some(hc) = hits.filter(|_| w > 3.0 && h > 3.0) {
            // 行が見えないほど小さいタイル: ヒット数を枠で示す
            let t = (hc.len() as f64).ln_1p() / 5.0;
            o.bg.add_colored_rect(self.rect(x, y, w, (h * t.min(1.0)).max(2.0)), a(HIT));
        }
        if w > 46.0 && h > 14.0 {
            let fs = (h * 0.4).min(12.0) as f32;
            let tag = match ft {
                Some(f) if f.binary => " (binary)",
                _ if !small_enough => " (large)",
                _ => "",
            };
            let label = format!("{}{}", n.name, tag);
            let lw = (label.chars().count() as f64 * fs as f64 * 0.55 + 6.0).min(w - 2.0);
            let lr = self.rect(x + 1.0, y + 1.0, lw, fs as f64 + 4.0);
            o.texts.push(TextOp {
                pos: Pos2::new((x + 4.0) as f32, (y + 2.0) as f32),
                text: label, size: fs.round(), mono: false, color: a(Color32::WHITE), clip: lr,
                bg: Some(a(Color32::from_black_alpha(140))),
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_image(&self, i: usize, kind: Kind, x: f64, y: f64, w: f64, h: f64, col: Color32, dim: bool, o: &mut DrawOut) -> bool {
        let id = i as u32;
        let n = &self.t.nodes[i];
        let ent = self.imgs.get(&id);
        if ent.is_some_and(|e| e.tex.is_none()) {
            return false; // 読めなかった → 通常タイル
        }
        let want = level_for(w.max(h));
        if w > 24.0 && h > 24.0 && ent.is_none_or(|e| e.level < want) {
            // まず最小解像度、表示が大きくなったら高解像度へ
            o.req.push((w * h, id, kind, if ent.is_none() { 0 } else { want }));
        }
        let a = |c: Color32| if dim { c.gamma_multiply(0.13) } else { c };
        let c = self.canvas;
        let (cminx, cmaxx, cminy, cmaxy) = (c.min.x as f64, c.max.x as f64, c.min.y as f64, c.max.y as f64);
        o.bg.add_colored_rect(self.rect(x, y, w, h), a(Color32::from_rgb(0x20, 0x20, 0x24)));
        let hdr = w.min(h) * 0.05;
        let m = w.min(h) * 0.02;
        let (ax, ay, aw, ah) = (x + m, y + hdr + m, w - 2.0 * m, h - hdr - 2.0 * m);
        let mut dims = String::new();
        if let Some(e) = ent
            && let Some(tex) = &e.tex
        {
            o.touched_img.push(id);
            dims = format!("  {}×{}{}", e.src.0, e.src.1, if e.note.is_empty() { String::new() } else { format!("  {}", e.note) });
            let (s0w, s0h) = (e.src.0.max(1) as f64, e.src.1.max(1) as f64);
            let sc = (aw / s0w).min(ah / s0h);
            let (iw, ih) = (s0w * sc, s0h * sc);
            let (ix, iy) = (ax + (aw - iw) / 2.0, ay + (ah - ih) / 2.0);
            if kind == Kind::Svg {
                o.bg.add_colored_rect(self.rect(ix, iy, iw, ih), a(Color32::from_gray(0xe8)));
            }
            // 見えている部分だけを UV で切り出す（深いズームでも f32 精度が破綻しない）
            let (vx0, vy0, vx1, vy1) = (ix.max(cminx), iy.max(cminy), (ix + iw).min(cmaxx), (iy + ih).min(cmaxy));
            if vx1 > vx0 && vy1 > vy0 && iw > 0.0 && ih > 0.0 {
                o.images.push(ImgOp {
                    tex: tex.id(),
                    rect: Rect::from_min_max(Pos2::new(vx0 as f32, vy0 as f32), Pos2::new(vx1 as f32, vy1 as f32)),
                    uv: Rect::from_min_max(
                        Pos2::new(((vx0 - ix) / iw) as f32, ((vy0 - iy) / ih) as f32),
                        Pos2::new(((vx1 - ix) / iw) as f32, ((vy1 - iy) / ih) as f32),
                    ),
                    tint: if dim { Color32::from_gray(33) } else { Color32::WHITE },
                });
            }
            if kind == Kind::Video && iw.min(ih) > 24.0 {
                // 再生マーク
                let r = (iw.min(ih) * 0.12).min(48.0) as f32;
                let cpos = Pos2::new((ix + iw / 2.0) as f32, (iy + ih / 2.0) as f32);
                if self.canvas.expand(r).contains(cpos) {
                    o.over.push(egui::Shape::circle_filled(cpos, r, a(Color32::from_black_alpha(150))));
                    let t = r * 0.5;
                    o.over.push(egui::Shape::convex_polygon(
                        vec![cpos + Vec2::new(-t * 0.6, -t), cpos + Vec2::new(t, 0.0), cpos + Vec2::new(-t * 0.6, t)],
                        a(Color32::WHITE),
                        egui::Stroke::NONE,
                    ));
                }
            }
        } else {
            o.bg.add_colored_rect(self.rect(ax, ay, aw, ah), a(col.gamma_multiply(0.35)));
        }
        let label = format!("{}{}  {}", n.name, dims, fmt_bytes(n.size as f64));
        if hdr >= 9.0 {
            o.bg.add_colored_rect(self.rect(x, y, w, hdr), a(col));
            let fs = (hdr * 0.7).min(200.0) as f32;
            o.texts.push(TextOp {
                pos: Pos2::new((x + 6.0).max(cminx) as f32, (y + hdr / 2.0 - fs as f64 * 0.6) as f32),
                text: label, size: fs.round(), mono: false, color: a(Color32::BLACK),
                clip: self.rect(x, y, w, hdr), bg: None,
            });
        } else if w > 46.0 && h > 14.0 {
            let fs = (h * 0.4).min(12.0) as f32;
            let lw = (n.name.chars().count() as f64 * fs as f64 * 0.55 + 6.0).min(w - 2.0);
            let lr = self.rect(x + 1.0, y + 1.0, lw, fs as f64 + 4.0);
            o.texts.push(TextOp {
                pos: Pos2::new((x + 4.0) as f32, (y + 2.0) as f32),
                text: n.name.clone(), size: fs.round(), mono: false, color: a(Color32::WHITE), clip: lr,
                bg: Some(a(Color32::from_black_alpha(140))),
            });
        }
        true
    }

    // ------------------------------------------------------------ minimap

    fn mm_rect(&self) -> Rect {
        let r = &self.t.nodes[self.root as usize];
        let w = (self.canvas.width() * 0.18).clamp(120.0, 260.0);
        let h = w * (r.h / r.w.max(1.0)) as f32;
        Rect::from_min_size(Pos2::new(self.canvas.max.x - w - 12.0, self.canvas.max.y - h - 12.0), Vec2::new(w, h))
    }

    fn mm_node(&self, i: usize, k: f64, org: Pos2, mesh: &mut Mesh) {
        let n = &self.t.nodes[i];
        let r = Rect::from_min_size(
            org + Vec2::new((n.x * k) as f32, (n.y * k) as f32),
            Vec2::new((n.w * k) as f32, (n.h * k) as f32),
        );
        if r.width() < 0.7 || r.height() < 0.7 {
            return;
        }
        let col = self.node_color(i);
        let col = if !n.is_dir && self.dim(i) { col.gamma_multiply(0.2) } else { col };
        if !n.is_dir || r.area() < 40.0 {
            mesh.add_colored_rect(r, col);
            return;
        }
        mesh.add_colored_rect(r, Color32::from_gray(0x18));
        for &c in &n.children {
            self.mm_node(c as usize, k, org, mesh);
        }
    }

    fn draw_minimap(&self, painter: &egui::Painter) {
        let mm = self.mm_rect();
        let k = mm.width() as f64 / self.t.nodes[self.root as usize].w.max(1e-9);
        let mut mesh = Mesh::default();
        mesh.add_colored_rect(mm.expand(4.0), Color32::from_black_alpha(220));
        let r = &self.t.nodes[self.root as usize];
        let org = mm.min - Vec2::new((r.x * k) as f32, (r.y * k) as f32);
        self.mm_node(self.root as usize, k, org, &mut mesh);
        painter.add(egui::Shape::mesh(mesh));
        // 現在の表示範囲
        let (cw, ch) = (self.canvas.width() as f64, self.canvas.height() as f64);
        let v0 = Pos2::new(mm.min.x + ((-self.tx / self.s) * k) as f32, mm.min.y + ((-self.ty / self.s) * k) as f32);
        let v1 = Pos2::new(mm.min.x + (((cw - self.tx) / self.s) * k) as f32, mm.min.y + (((ch - self.ty) / self.s) * k) as f32);
        let mut v = Rect::from_min_max(v0, v1).intersect(mm.expand(2.0));
        if v.width() < 4.0 || v.height() < 4.0 {
            v = Rect::from_center_size(v.center(), Vec2::splat(6.0)); // 深いズーム時も位置が分かるように
        }
        let w = 1.5;
        for e in [
            Rect::from_min_max(v.min, Pos2::new(v.max.x, v.min.y + w)),
            Rect::from_min_max(Pos2::new(v.min.x, v.max.y - w), v.max),
            Rect::from_min_max(v.min, Pos2::new(v.min.x + w, v.max.y)),
            Rect::from_min_max(Pos2::new(v.max.x - w, v.min.y), v.max),
        ] {
            painter.rect_filled(e, 0.0, Color32::from_rgb(0xff, 0xe0, 0x40));
        }
    }

    fn mm_to_world(&self, p: Pos2) -> (f64, f64) {
        let mm = self.mm_rect();
        let k = mm.width() as f64 / self.t.nodes[self.root as usize].w.max(1e-9);
        let r = &self.t.nodes[self.root as usize];
        (r.x + ((p.x - mm.min.x) as f64) / k, r.y + ((p.y - mm.min.y) as f64) / k)
    }

    fn draw(&mut self, painter: &egui::Painter) {
        let mut o = DrawOut {
            bg: Mesh::default(), images: vec![], over: vec![], texts: vec![], req: vec![], touched: vec![],
            touched_img: vec![], budget: 120_000,
        };
        o.bg.add_colored_rect(self.canvas, Color32::from_rgb(0x1e, 0x1e, 0x1e));
        let d0 = self.depth(self.root);
        self.draw_node(self.root as usize, d0, &mut o);
        painter.add(egui::Shape::mesh(o.bg));
        for im in o.images {
            painter.with_clip_rect(self.canvas).image(im.tex, im.rect, im.uv, im.tint);
        }
        painter.with_clip_rect(self.canvas).extend(o.over);
        for t in o.texts {
            if let Some(b) = t.bg {
                painter.with_clip_rect(self.canvas).rect_filled(t.clip, 0.0, b);
            }
            let font = if t.mono { FontId::monospace(t.size) } else { FontId::proportional(t.size) };
            painter.with_clip_rect(t.clip.intersect(self.canvas)).text(t.pos, Align2::LEFT_TOP, t.text, font, t.color);
        }
        if let Some(hv) = self.hover {
            let (sx, sy, sw, sh) = self.to_screen(&self.t.nodes[hv as usize]);
            let r = self.rect(sx, sy, sw, sh);
            for e in [
                Rect::from_min_max(r.min, Pos2::new(r.max.x, r.min.y + 2.0)),
                Rect::from_min_max(Pos2::new(r.min.x, r.max.y - 2.0), r.max),
                Rect::from_min_max(r.min, Pos2::new(r.min.x + 2.0, r.max.y)),
                Rect::from_min_max(Pos2::new(r.max.x - 2.0, r.min.y), r.max),
            ] {
                painter.rect_filled(e, 0.0, Color32::WHITE);
            }
        }
        // ロード要求とキャッシュ管理
        self.frame += 1;
        for id in o.touched {
            if let Some(f) = self.cache.get_mut(&id) {
                f.last = self.frame;
            }
        }
        for id in o.touched_img {
            if let Some(e) = self.imgs.get_mut(&id) {
                e.last = self.frame;
            }
        }
        {
            // 今見えているものだけでキューを作り直す（面積の大きいものが先）
            let (m, cv) = &*self.shared;
            let mut g = m.lock().unwrap();
            g.queue.clear();
            let mut req = o.req;
            req.sort_by(|a, b| a.0.total_cmp(&b.0));
            let start = req.len().saturating_sub(3000);
            for &(_, id, kind, level) in &req[start..] {
                if !g.inflight.contains(&id) {
                    g.queue.push(Job { id, path: self.t.path(&self.opts.root, id), kind, level });
                }
            }
            if !g.queue.is_empty() {
                cv.notify_all();
            }
        }
        if self.img_bytes > IMG_LIMIT {
            let mut v: Vec<(u64, u32)> =
                self.imgs.iter().filter(|(_, e)| e.tex.is_some()).map(|(k, e)| (e.last, *k)).collect();
            v.sort();
            for (_, k) in v {
                if self.img_bytes < IMG_LIMIT * 3 / 4 {
                    break;
                }
                if let Some(e) = self.imgs.remove(&k) {
                    self.img_bytes -= e.bytes;
                }
            }
        }
        if self.cache_bytes > CACHE_LIMIT {
            let mut v: Vec<(u64, u32)> = self.cache.iter().map(|(k, f)| (f.last, *k)).collect();
            v.sort();
            for (_, k) in v {
                if self.cache_bytes < CACHE_LIMIT * 3 / 4 {
                    break;
                }
                if let Some(f) = self.cache.remove(&k) {
                    self.cache_bytes -= f.bytes;
                }
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        // ---- バックグラウンド結果の取り込み
        let mut got = false;
        while let Ok(r) = self.res_rx.try_recv() {
            got = true;
            match r {
                Loaded::Text(id, ft) => {
                    if let Some(f) = ft {
                        self.cache_bytes += f.bytes;
                        if let Some(old) = self.cache.insert(id, f) {
                            self.cache_bytes -= old.bytes;
                        }
                    } else {
                        // 読めない/大きすぎる: 空エントリで再要求を防ぐ
                        self.cache.insert(id, FileText { lines: vec![], ind: vec![], len: vec![], maxlen: 0, binary: false, bytes: 0, last: 0, md: None });
                    }
                    if let Some((pid, line)) = self.pending_jump {
                        if pid == id {
                            self.jump_to_line(id, line, now);
                        }
                    }
                }
                Loaded::Image(id, level, res) => {
                    let cur = self.imgs.get_mut(&id);
                    match (res, cur) {
                        (None, Some(e)) if e.tex.is_some() => e.level = LEVELS.len() - 1, // 高解像度化失敗: 現状維持
                        (None, _) => {
                            self.imgs.insert(id, ImgEntry { tex: None, level, src: (0, 0), note: String::new(), bytes: 0, last: 0 });
                        }
                        (Some(_), Some(e)) if e.tex.is_some() && e.level > level => {}
                        (Some((ci, src, note)), _) => {
                            let bytes = ci.size[0] * ci.size[1] * 4;
                            let tex = ctx.load_texture(format!("img{id}"), ci, egui::TextureOptions::LINEAR);
                            let e = ImgEntry { tex: Some(tex), level, src, note, bytes, last: self.frame };
                            if let Some(old) = self.imgs.insert(id, e) {
                                self.img_bytes -= old.bytes;
                            }
                            self.img_bytes += bytes;
                        }
                    }
                }
            }
        }
        if let Ok(r) = self.git_rx.try_recv() {
            match r {
                Ok(st) => self.apply_git(st),
                Err(e) => self.git_status = format!("git: {e}"),
            }
        }
        let cur_gen = self.search_gen.load(std::sync::atomic::Ordering::SeqCst);
        while let Ok(b) = self.search_rx.try_recv() {
            if b.generation != cur_gen {
                continue;
            }
            for (id, lines) in b.hits {
                if let Some(q) = self.qmatch.get_mut(id as usize) {
                    *q = true;
                }
                self.search_hits.insert(id, lines);
            }
            let total = self.search_files.as_ref().map_or(0, |f| f.len());
            let mut order: Vec<u32> = self.search_hits.keys().copied().collect();
            order.sort_by_key(|id| (std::cmp::Reverse(self.search_hits[id].len()), *id));
            self.hit_order = order;
            self.search_status = format!(
                "{}{} files / {} lines",
                if b.done { String::new() } else { format!("searching… {}/{}  ", b.scanned, total) },
                self.search_hits.len(),
                self.total_hits()
            );
        }
        if let Some(t) = self.search_dirty_at {
            if now - t > 0.3 {
                self.search_dirty_at = None;
                self.start_search();
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        // ---- top bar
        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("tilescope");
                let before = self.metric;
                egui::ComboBox::from_id_salt("metric")
                    .selected_text(format!("Size: {}", self.metric.name()))
                    .show_ui(ui, |ui| {
                        for (m, name) in Metric::ALL {
                            ui.selectable_value(&mut self.metric, m, name);
                        }
                    });
                if before != self.metric {
                    self.recompute_values();
                    self.focus = self.root;
                }
                egui::ComboBox::from_id_salt("color")
                    .selected_text(format!("Color: {}", self.color_mode.name()))
                    .show_ui(ui, |ui| {
                        for (m, name) in ColorMode::ALL {
                            ui.selectable_value(&mut self.color_mode, m, name);
                        }
                    });
                ui.separator();
                let prev = self.search_content;
                ui.selectable_value(&mut self.search_content, false, "Path");
                ui.selectable_value(&mut self.search_content, true, "Content");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text(if self.search_content { "Search contents (Enter: next hit)" } else { "Filter by path (Enter: next)" })
                        .desired_width(220.0),
                );
                if r.changed() || prev != self.search_content {
                    self.hit_pos = 0;
                    if self.search_content {
                        self.search_dirty_at = Some(now);
                    } else {
                        self.search_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        self.search_hits.clear();
                        self.search_status.clear();
                        self.update_query();
                    }
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    if self.search_dirty_at.take().is_some() {
                        self.start_search();
                    }
                    self.next_hit(now);
                    r.request_focus();
                }
                if !self.search_status.is_empty() {
                    ui.label(egui::RichText::new(&self.search_status).color(HIT));
                }
                ui.checkbox(&mut self.show_minimap, "Minimap");
                ui.separator();
                if ui.add_enabled(!self.back.is_empty(), egui::Button::new("◀")).on_hover_text("Back (Alt+Left / mouse back)").clicked() {
                    self.go_back(now);
                }
                if ui.add_enabled(!self.fwd.is_empty(), egui::Button::new("▶")).on_hover_text("Forward (Alt+Right / mouse forward)").clicked() {
                    self.go_fwd(now);
                }
                let has_parent = self.t.nodes[self.root as usize].parent != NONE;
                if ui.add_enabled(has_parent, egui::Button::new("⬆")).on_hover_text("Up one level (Backspace)").clicked() {
                    self.go_up(now);
                }
                // パンくず: 先頭〜表示ルート〜ズーム中の位置。クリックでそのディレクトリだけを表示
                let mut chain = vec![];
                let mut i = if self.is_under(self.focus, self.root) { self.focus } else { self.root };
                while i != NONE {
                    chain.push(i);
                    i = self.t.nodes[i as usize].parent;
                }
                let root_depth = self.depth(self.root);
                for (k, &id) in chain.iter().rev().enumerate() {
                    if k > 0 {
                        ui.label(egui::RichText::new("›").weak());
                    }
                    let n = &self.t.nodes[id as usize];
                    let mut txt = egui::RichText::new(&n.name);
                    if id == self.root {
                        txt = txt.strong().color(Color32::WHITE).background_color(HEADER_COLORS[root_depth % HEADER_COLORS.len()]);
                    } else if !self.is_under(id, self.root) {
                        txt = txt.color(Color32::from_gray(0x90));
                    }
                    let hint = if n.is_dir { "Show only this folder" } else { "Zoom to this file" };
                    if ui.link(txt).on_hover_text(hint).clicked() {
                        if n.is_dir {
                            self.set_root(id, now, true);
                        } else {
                            self.fit(id, now, false);
                        }
                    }
                }
                ui.label(self.fmt_val(self.t.nodes[self.focus as usize].val));
                if let Some((msg, t)) = &self.status {
                    if now - t < 4.0 {
                        ui.label(egui::RichText::new(msg).color(Color32::from_rgb(0x9c, 0xdc, 0xfe)));
                        ctx.request_repaint_after(std::time::Duration::from_millis(500));
                    }
                }
            });
        });

        // ---- side panel
        egui::Panel::right("side").default_size(240.0).show(ui, |ui| {
            ui.label(self.t.path(&self.opts.root, self.root).display().to_string());
            ui.label(format!("{}  /  {} files", self.fmt_val(self.total), self.ext_stats.iter().map(|e| e.2).sum::<usize>()));
            ui.label(egui::RichText::new(&self.git_status).weak());
            if self.color_mode != ColorMode::Ext {
                ui.separator();
                let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 14.0), Sense::hover());
                let mut mesh = Mesh::default();
                let steps = 48;
                for k in 0..steps {
                    let x0 = r.min.x + r.width() * k as f32 / steps as f32;
                    let x1 = r.min.x + r.width() * (k + 1) as f32 / steps as f32;
                    mesh.add_colored_rect(
                        Rect::from_min_max(Pos2::new(x0, r.min.y), Pos2::new(x1, r.max.y)),
                        ramp(k as f32 / (steps - 1) as f32),
                    );
                }
                ui.painter().add(egui::Shape::mesh(mesh));
                ui.horizontal(|ui| {
                    let (lo, hi) = match self.color_mode {
                        ColorMode::Age if self.git_newest > 0 => (git::ymd(self.git_oldest), git::ymd(self.git_newest)),
                        ColorMode::Churn => ("1".into(), format!("{} commits", self.git_max_count)),
                        _ => ("old".into(), "new".into()),
                    };
                    ui.label(lo);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(hi));
                });
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    ui.painter().rect_filled(r, 2.0, NOGIT);
                    ui.label("no history / untracked");
                });
            }
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (e, v, cnt) in self.ext_stats.clone() {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                        ui.painter().rect_filled(r, 2.0, self.t.ext_colors[e as usize]);
                        let sel = self.hl_ext == Some(e);
                        let txt = format!(".{} ({})  {:.1}%", self.t.ext_names[e as usize], cnt, v / self.total * 100.0);
                        if ui.selectable_label(sel, txt).on_hover_text(self.fmt_val(v)).clicked() {
                            self.hl_ext = if sel { None } else { Some(e) };
                        }
                    });
                }
            });
        });

        // ---- canvas
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
            let first = self.canvas == Rect::NOTHING;
            self.canvas = resp.rect;
            if self.laid_size != self.canvas.size() {
                self.relayout();
                let f = if first { self.root } else { self.focus };
                self.fit(f, now, true);
            }
            if self.step_anim(now) {
                ctx.request_repaint();
            }
            // ミニマップは全体より拡大しているときだけ表示
            let mm_visible = self.show_minimap && self.s > self.view_for(self.root).0 * 1.3;
            let mm = mm_visible.then(|| self.mm_rect());
            let ptr = resp.interact_pointer_pos().or(resp.hover_pos());
            let in_mm = |p: Option<Pos2>| mm.zip(p).is_some_and(|(m, p)| m.expand(4.0).contains(p));

            // ---- ミニマップ操作（クリック/ドラッグで移動）
            if resp.drag_started() {
                self.mm_drag = in_mm(ptr);
            }
            if !resp.dragged() && !resp.drag_started() && !ctx.input(|i| i.pointer.any_down()) {
                self.mm_drag = false;
            }
            let mm_active = self.mm_drag || (resp.clicked() && in_mm(ptr));
            if mm_active {
                if let Some(p) = ptr {
                    let w = self.mm_to_world(p);
                    self.anim = None;
                    self.set_center(self.s, w);
                }
            }

            // zoom
            if resp.hovered() {
                let (scroll, zoom) = ctx.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
                let f = (scroll as f64 * 0.003).exp() * zoom as f64;
                if (f - 1.0).abs() > 1e-6 {
                    if let Some(p) = resp.hover_pos() {
                        self.anim = None;
                        let (mx, my) = ((p.x - self.canvas.min.x) as f64, (p.y - self.canvas.min.y) as f64);
                        let (wx, wy) = ((mx - self.tx) / self.s, (my - self.ty) / self.s);
                        self.s = (self.s * f).clamp(0.05, 1e9);
                        self.tx = mx - wx * self.s;
                        self.ty = my - wy * self.s;
                    }
                }
            }
            if resp.dragged() && !self.mm_drag {
                self.anim = None;
                let d = resp.drag_delta();
                self.tx += d.x as f64;
                self.ty += d.y as f64;
            }
            let hp = resp.hover_pos().filter(|p| !in_mm(Some(*p)));
            self.hover = hp.and_then(|p| {
                self.t.hit(self.root as usize, ((p.x - self.canvas.min.x) as f64 - self.tx) / self.s, ((p.y - self.canvas.min.y) as f64 - self.ty) / self.s)
            });
            self.hover_header = match (self.hover, hp) {
                (Some(h), Some(p)) => {
                    let n = &self.t.nodes[h as usize];
                    let wy = ((p.y - self.canvas.min.y) as f64 - self.ty) / self.s;
                    n.is_dir && n.hdr > 0.0 && wy <= n.y + n.hdr && n.hdr * self.s >= 4.0
                }
                _ => false,
            };
            if self.hover_header {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() && !in_mm(ptr) && self.hover_header {
                if let Some(h) = self.hover {
                    self.set_root(h, now, true);
                }
            }
            self.hover_line = match (self.hover, hp) {
                (Some(h), Some(p)) if !self.t.nodes[h as usize].is_dir => self.line_at(h, p),
                _ => None,
            };
            if resp.double_clicked() && !in_mm(ptr) && !self.hover_header && now - self.root_changed_at > 0.6 {
                if let Some(h) = self.hover {
                    self.fit(h, now, false);
                }
            }
            if resp.secondary_clicked() {
                self.menu_target = self.hover.map(|h| (h, self.hover_line));
            }
            if !ctx.egui_wants_keyboard_input() {
                let (up, back, fwd) = ctx.input(|i| {
                    (
                        i.key_pressed(Key::Backspace) || (i.modifiers.alt && i.key_pressed(Key::ArrowUp)),
                        (i.modifiers.alt && i.key_pressed(Key::ArrowLeft)) || i.pointer.button_pressed(egui::PointerButton::Extra1),
                        (i.modifiers.alt && i.key_pressed(Key::ArrowRight)) || i.pointer.button_pressed(egui::PointerButton::Extra2),
                    )
                });
                if up {
                    self.go_up(now);
                }
                if back {
                    self.go_back(now);
                }
                if fwd {
                    self.go_fwd(now);
                }
                let (esc, home, open_key, mm_key, next_key) = ctx.input(|i| {
                    (
                        i.key_pressed(Key::Escape),
                        i.key_pressed(Key::Num0) || i.key_pressed(Key::Home),
                        i.key_pressed(Key::O),
                        i.key_pressed(Key::M),
                        i.key_pressed(Key::N) || i.key_pressed(Key::Enter),
                    )
                });
                if next_key {
                    self.next_hit(now);
                }
                if esc {
                    // ズームを 1 段戻す。表示ルート全体を見ているなら 1 階層上へ
                    if self.focus == self.root || !self.is_under(self.focus, self.root) {
                        self.go_up(now);
                    } else {
                        let p = self.t.nodes[self.focus as usize].parent;
                        self.fit(p, now, false);
                    }
                }
                if home {
                    self.fit(self.root, now, false);
                }
                if mm_key {
                    self.show_minimap = !self.show_minimap;
                }
                if open_key {
                    if let Some(h) = self.hover {
                        let p = self.t.path(&self.opts.root, h);
                        let r = if self.t.nodes[h as usize].is_dir { open::open_default(&p) } else { open::open_in_editor(&p, self.hover_line.map(|l| l + 1)) };
                        self.flash(match r { Ok(()) => format!("Opened: {}", p.display()), Err(e) => format!("Failed: {e}") });
                    }
                }
            }

            self.draw(&painter);
            if mm_visible {
                self.draw_minimap(&painter);
            }

            // ---- 右クリックメニュー
            resp.context_menu(|ui| {
                let Some((id, line)) = self.menu_target else {
                    ui.label("(nothing here)");
                    return;
                };
                let p = self.t.path(&self.opts.root, id);
                let is_dir = self.t.nodes[id as usize].is_dir;
                ui.label(egui::RichText::new(&self.t.nodes[id as usize].name).strong());
                ui.separator();
                let mut result: Option<Result<(), String>> = None;
                if ui.button(if is_dir { "Open folder" } else { "Open with default app" }).clicked() {
                    result = Some(open::open_default(&p));
                }
                if !is_dir {
                    let label = match line {
                        Some(l) => format!("Open in editor (line {})", l + 1),
                        None => "Open in editor".into(),
                    };
                    if ui.button(label).clicked() {
                        result = Some(open::open_in_editor(&p, line.map(|l| l + 1)));
                    }
                }
                let label = if is_dir { "Show only this folder" } else { "Show containing folder" };
                if ui.button(label).clicked() {
                    self.set_root(id, now, true);
                    result = Some(Ok(()));
                }
                if ui.button("Show in file manager").clicked() {
                    result = Some(open::reveal(&p));
                }
                ui.separator();
                if ui.button("Copy path").clicked() {
                    ctx.copy_text(p.display().to_string());
                    result = Some(Ok(()));
                    self.flash("Path copied");
                }
                if ui.button("Copy relative path").clicked() {
                    let rel = p.strip_prefix(&self.opts.root).unwrap_or(&p).display().to_string();
                    ctx.copy_text(rel);
                    result = Some(Ok(()));
                    self.flash("Relative path copied");
                }
                if ui.button("Zoom here").clicked() {
                    self.fit(id, now, false);
                    result = Some(Ok(()));
                }
                if let Some(r) = result {
                    if let Err(e) = r {
                        self.flash(format!("Failed: {e}"));
                    }
                    ui.close();
                }
            });

            // ---- tooltip
            if let (Some(h), Some(p)) = (self.hover, hp) {
                if !resp.dragged() {
                    let n = &self.t.nodes[h as usize];
                    let mut extra = if n.is_dir {
                        String::new()
                    } else if let Some(e) = self.imgs.get(&h).filter(|e| e.tex.is_some()) {
                        format!("  {}×{}  {}  {}", e.src.0, e.src.1, e.note, fmt_bytes(n.size as f64))
                    } else if self.metric != Metric::Bytes {
                        format!("  {}", fmt_bytes(n.size as f64))
                    } else {
                        match self.cache.get(&h) {
                            Some(f) if !f.lines.is_empty() => format!("  {} lines", f.lines.len()),
                            _ => String::new(),
                        }
                    };
                    if let Some(l) = self.hover_line {
                        extra += &format!("  [line {}]", l + 1);
                    }
                    if n.git_last > 0 {
                        extra += &format!("\ngit: last modified {} / {} commits", git::ymd(n.git_last), n.git_count);
                    }
                    if let Some(hc) = self.search_hits.get(&h) {
                        extra += &format!("\nsearch hits: {} lines", hc.len());
                    }
                    let text = format!("{}\n{}{}", self.t.path(&self.opts.root, h).display(), self.fmt_val(n.val), extra);
                    let g = painter.layout_no_wrap(text, FontId::monospace(12.0), Color32::WHITE);
                    let mut pos = p + Vec2::new(14.0, 16.0);
                    pos.x = pos.x.min(self.canvas.max.x - g.size().x - 10.0).max(self.canvas.min.x);
                    if pos.y + g.size().y + 8.0 > self.canvas.max.y {
                        pos.y = p.y - g.size().y - 16.0;
                    }
                    let r = Rect::from_min_size(pos, g.size()).expand(5.0);
                    painter.rect_filled(r, 3.0, Color32::from_black_alpha(230));
                    painter.galley(pos, g, Color32::WHITE);
                }
            }
        });
        if got {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }
}

fn main() -> eframe::Result {
    let opts = parse_args();
    let title = format!("tilescope — {}", opts.root.display());
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1400.0, 900.0]).with_title(&title),
        ..Default::default()
    };
    eframe::run_native(
        "tilescope",
        native,
        Box::new(move |cc| {
            add_cjk_font(&cc.egui_ctx);
            Ok(Box::new(App::new(opts, cc.egui_ctx.clone())))
        }),
    )
}

/// 日本語などを表示するため、fontconfig で見つかった CJK フォントをフォールバックに追加
fn add_cjk_font(ctx: &egui::Context) {
    let Ok(out) = std::process::Command::new("fc-match").args(["-f", "%{file}", ":lang=ja"]).output() else { return };
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let Ok(bytes) = std::fs::read(&path) else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("cjk".into(), Arc::new(egui::FontData::from_owned(bytes)));
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("cjk".into());
    }
    ctx.set_fonts(fonts);
}
