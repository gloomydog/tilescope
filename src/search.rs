//! ファイル内容の全文検索（バックグラウンド・世代番号でキャンセル）

use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

pub struct Batch {
    pub generation: u64,
    /// (node id, ヒット行番号(0始まり)の昇順)
    pub hits: Vec<(u32, Vec<u32>)>,
    pub scanned: usize,
    pub done: bool,
}

const MAX_HITS_PER_FILE: usize = 5000;

fn search_file(path: &PathBuf, q: &str, ci: bool) -> Option<Vec<u32>> {
    let data = std::fs::read(path).ok()?;
    if data[..data.len().min(8192)].contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&data);
    let hay = if ci { std::borrow::Cow::Owned(text.to_lowercase()) } else { text };
    let mut lines = vec![];
    let (mut line, mut pos) = (0u32, 0usize);
    let bytes = hay.as_bytes();
    for (i, _) in hay.match_indices(q) {
        line += bytes[pos..i].iter().filter(|&&b| b == b'\n').count() as u32;
        pos = i;
        if lines.last() != Some(&line) {
            lines.push(line);
            if lines.len() >= MAX_HITS_PER_FILE {
                break;
            }
        }
    }
    if lines.is_empty() { None } else { Some(lines) }
}

/// smart case: クエリに大文字が含まれる場合のみ大文字小文字を区別
pub fn spawn(
    generation: u64,
    files: Arc<Vec<(u32, PathBuf)>>,
    query: String,
    current: Arc<AtomicU64>,
    tx: mpsc::Sender<Batch>,
    repaint: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let ci = !query.chars().any(|c| c.is_uppercase());
        let q = if ci { query.to_lowercase() } else { query };
        let mut scanned = 0;
        for chunk in files.chunks(512) {
            if current.load(Ordering::Relaxed) != generation {
                return;
            }
            let hits: Vec<(u32, Vec<u32>)> =
                chunk.par_iter().filter_map(|(id, p)| search_file(p, &q, ci).map(|l| (*id, l))).collect();
            scanned += chunk.len();
            if tx.send(Batch { generation, hits, scanned, done: false }).is_err() {
                return;
            }
            repaint();
        }
        let _ = tx.send(Batch { generation, hits: vec![], scanned, done: true });
        repaint();
    });
}
