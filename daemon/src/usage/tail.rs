//! Incremental reading of append-only JSONL logs: remembers a byte offset per file
//! and only hands complete new lines to the caller.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Duration, Utc};
use tracing::warn;

use super::RETENTION_DAYS;

pub struct JsonlTail {
    root: PathBuf,
    /// Bytes consumed per file (always at a line boundary).
    offsets: HashMap<PathBuf, u64>,
}

impl JsonlTail {
    pub fn new(root: PathBuf) -> Self {
        Self { root, offsets: HashMap::new() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Calls `on_line(path, line)` for each complete line appended since the last scan.
    /// Untouched files older than the retention window are skipped on first sight.
    pub fn scan(&mut self, now: DateTime<Utc>, mut on_line: impl FnMut(&Path, &[u8])) {
        let cutoff = SystemTime::from(now - Duration::days(RETENTION_DAYS));
        let mut files = Vec::new();
        collect_jsonl(&self.root, &mut files);

        for path in files {
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let known = self.offsets.get(&path).copied();
            if known.is_none() && meta.modified().is_ok_and(|m| m < cutoff) {
                continue;
            }
            let mut offset = known.unwrap_or(0);
            if meta.len() < offset {
                offset = 0; // truncated or replaced; record keys make re-reading safe
            }
            if meta.len() > offset {
                match read_lines_from(&path, offset, |line| on_line(&path, line)) {
                    Ok(consumed) => offset += consumed,
                    Err(e) => warn!("reading {}: {e}", path.display()),
                }
            }
            self.offsets.insert(path, offset);
        }
        self.offsets.retain(|p, _| p.exists());
    }
}

/// Returns bytes consumed. A trailing partial line is still being written; it is
/// left for the next scan.
fn read_lines_from(path: &Path, offset: u64, mut on_line: impl FnMut(&[u8])) -> std::io::Result<u64> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { return Ok(0) };
    for line in buf[..end].split(|&b| b == b'\n') {
        if !line.is_empty() {
            on_line(line);
        }
    }
    Ok(end as u64 + 1)
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => collect_jsonl(&path, out),
            Ok(ft) if ft.is_file() && path.extension().is_some_and(|e| e == "jsonl") => out.push(path),
            _ => {}
        }
    }
}

/// Cheap prefilter before JSON parsing.
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tails_appended_lines_and_waits_for_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        std::fs::write(&path, "one\n").unwrap();
        let now = Utc::now();
        let mut tail = JsonlTail::new(dir.path().to_owned());
        let mut seen = Vec::new();

        tail.scan(now, |_, l| seen.push(String::from_utf8_lossy(l).into_owned()));
        assert_eq!(seen, ["one"]);

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"tw").unwrap();
        tail.scan(now, |_, l| seen.push(String::from_utf8_lossy(l).into_owned()));
        assert_eq!(seen, ["one"]);

        f.write_all(b"o\n").unwrap();
        tail.scan(now, |_, l| seen.push(String::from_utf8_lossy(l).into_owned()));
        assert_eq!(seen, ["one", "two"]);
    }
}
