//! Incremental reading of append-only JSONL logs: remembers a byte offset per file
//! and only hands complete new lines to the caller.
//!
//! A file counts as the same stream while its inode is unchanged, it hasn't shrunk
//! and its first bytes still match. Otherwise (replaced, truncated, rewritten in
//! place) it is read again from the start and the caller gets `Event::Reset` first;
//! record keys make re-reading identical content harmless.

use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Duration, Utc};
use tracing::warn;

use super::RETENTION_DAYS;

/// Bytes of the file start remembered to detect in-place rewrites.
const HEAD_LEN: usize = 256;

pub enum Event<'a> {
    /// The file is being read from the start again; drop per-file parser state.
    Reset(&'a Path),
    Line(&'a Path, &'a [u8]),
}

struct FileState {
    dev: u64,
    ino: u64,
    mtime: Option<SystemTime>,
    /// Bytes consumed (always at a line boundary).
    offset: u64,
    head: Vec<u8>,
}

pub struct JsonlTail {
    root: PathBuf,
    files: HashMap<PathBuf, FileState>,
}

impl JsonlTail {
    pub fn new(root: PathBuf) -> Self {
        Self { root, files: HashMap::new() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Calls `on_line(path, line)` for each complete line appended since the last scan.
    pub fn scan(&mut self, now: DateTime<Utc>, mut on_line: impl FnMut(&Path, &[u8])) {
        self.scan_events(now, |ev| {
            if let Event::Line(path, line) = ev {
                on_line(path, line)
            }
        });
    }

    /// Like `scan`, but also reports streams that restart from the beginning.
    /// Untouched files older than the retention window are skipped on first sight.
    pub fn scan_events(&mut self, now: DateTime<Utc>, mut on_event: impl FnMut(Event)) {
        let cutoff = SystemTime::from(now - Duration::days(RETENTION_DAYS));
        let mut paths = Vec::new();
        collect_jsonl(&self.root, &mut paths);

        for path in paths {
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let mtime = meta.modified().ok();
            let known = self.files.get(&path);
            if known.is_none() && mtime.is_some_and(|m| m < cutoff) {
                continue;
            }
            if known.is_some_and(|f| f.unchanged(&meta)) {
                continue;
            }
            let mut offset = match known {
                Some(f) if f.continues(&path, &meta) => f.offset,
                Some(_) => {
                    on_event(Event::Reset(&path));
                    0
                }
                None => 0,
            };
            match read_lines_from(&path, offset, |line| on_event(Event::Line(&path, line))) {
                Ok(consumed) => offset += consumed,
                Err(e) => warn!("reading {}: {e}", path.display()),
            }
            let head = match known {
                Some(f) if f.offset > 0 && offset > 0 && f.head.len() == HEAD_LEN => f.head.clone(),
                _ => read_head(&path).unwrap_or_default(),
            };
            self.files.insert(path, FileState { dev: meta.dev(), ino: meta.ino(), mtime, offset, head });
        }
        self.files.retain(|p, _| p.exists());
    }
}

impl FileState {
    fn same_file(&self, meta: &Metadata) -> bool {
        self.dev == meta.dev() && self.ino == meta.ino()
    }

    /// Nothing to read and no sign of a rewrite.
    fn unchanged(&self, meta: &Metadata) -> bool {
        self.same_file(meta) && meta.len() == self.offset && meta.modified().ok() == self.mtime
    }

    /// Whether reading can resume at `offset`.
    fn continues(&self, path: &Path, meta: &Metadata) -> bool {
        self.same_file(meta)
            && meta.len() >= self.offset
            && read_head(path).is_ok_and(|h| h.starts_with(&self.head) || self.head.starts_with(&h))
    }
}

fn read_head(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(HEAD_LEN);
    File::open(path)?.take(HEAD_LEN as u64).read_to_end(&mut buf)?;
    Ok(buf)
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

    /// Lines read in one scan, with "RESET" marking a restarted stream.
    fn events(tail: &mut JsonlTail) -> Vec<String> {
        let mut out = Vec::new();
        tail.scan_events(Utc::now(), |ev| {
            out.push(match ev {
                Event::Reset(_) => "RESET".into(),
                Event::Line(_, l) => String::from_utf8_lossy(l).into_owned(),
            })
        });
        out
    }

    /// Replace `path` atomically (new inode), like an editor or `mv` would.
    fn replace(path: &Path, content: &str) {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, content).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    #[test]
    fn rereads_replaced_and_rewritten_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        std::fs::write(&path, "aaa\nbbb\n").unwrap();
        let mut tail = JsonlTail::new(dir.path().to_owned());
        assert_eq!(events(&mut tail), ["aaa", "bbb"]);
        assert!(events(&mut tail).is_empty());

        // Equal-size replacement.
        replace(&path, "ccc\nddd\n");
        assert_eq!(events(&mut tail), ["RESET", "ccc", "ddd"]);

        // Larger replacement: read from the start, not from the old offset.
        replace(&path, "eee\nfff\nggg\n");
        assert_eq!(events(&mut tail), ["RESET", "eee", "fff", "ggg"]);

        // Truncated in place, then written again beyond the old offset before we look.
        std::fs::write(&path, "hhh\niii\njjj\nkkk\n").unwrap();
        assert_eq!(events(&mut tail), ["RESET", "hhh", "iii", "jjj", "kkk"]);

        // Truncated and seen while shorter, then appended to.
        std::fs::write(&path, "lll\n").unwrap();
        assert_eq!(events(&mut tail), ["RESET", "lll"]);
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"mmm\n").unwrap();
        assert_eq!(events(&mut tail), ["mmm"]);
    }
}
