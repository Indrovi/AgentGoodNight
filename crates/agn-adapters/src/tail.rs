//! Incremental JSONL tailing that stays cheap on very large files.
//!
//! Agent transcripts grow to hundreds of megabytes. A file is never read in
//! full: the first read starts `initial_window` bytes before the end, later
//! reads continue from the last offset. Zstandard-compressed rollouts
//! (`*.jsonl.zst`) are decoded whole, but only when their size or mtime
//! changed and only below a size cap.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use agn_core::Millis;

/// Upper bound read from a plain file per poll; the rest follows next poll.
const MAX_CHUNK: u64 = 8 * 1024 * 1024;
/// Compressed files larger than this are not decoded.
const MAX_ZST: u64 = 1024 * 1024 * 1024;

pub fn mtime_millis(meta: &fs::Metadata) -> Millis {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as Millis).unwrap_or(0)
}

#[derive(Debug)]
pub struct FileTail {
    pub path: PathBuf,
    offset: u64,
    len_seen: u64,
    mtime: Millis,
    started: bool,
    partial: Vec<u8>,
    compressed: bool,
    /// For compressed files: number of decoded lines already emitted.
    lines_emitted: usize,
}

#[derive(Debug, Default)]
pub struct TailRead {
    pub lines: Vec<String>,
    /// File modification time when it was read.
    pub mtime: Millis,
    /// True when this read began mid-file (history before it is unknown).
    pub from_middle: bool,
    pub grew: bool,
}

impl FileTail {
    pub fn new(path: PathBuf) -> Self {
        let compressed = path.extension().is_some_and(|e| e == "zst");
        Self {
            path,
            offset: 0,
            len_seen: 0,
            mtime: 0,
            started: false,
            partial: Vec::new(),
            compressed,
            lines_emitted: 0,
        }
    }

    pub fn mtime(&self) -> Millis {
        self.mtime
    }

    /// Reads complete lines appended since the last call.
    pub fn read_new(&mut self, initial_window: u64) -> io::Result<TailRead> {
        let meta = fs::metadata(&self.path)?;
        let len = meta.len();
        let mtime = mtime_millis(&meta);
        if self.started && len == self.len_seen && mtime == self.mtime {
            return Ok(TailRead { mtime, ..Default::default() });
        }
        let grew = self.started && (len != self.len_seen || mtime != self.mtime);
        self.mtime = mtime;
        if self.compressed {
            return self.read_compressed(len, mtime, initial_window, grew);
        }

        let mut from_middle = false;
        if !self.started || len < self.offset {
            // First read, or the file was truncated / replaced.
            self.offset = len.saturating_sub(initial_window);
            from_middle = self.offset > 0;
            self.partial.clear();
            self.started = true;
        }
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(self.offset))?;
        let want = (len - self.offset).min(MAX_CHUNK);
        let mut buf = Vec::with_capacity(want as usize);
        f.take(want).read_to_end(&mut buf)?;
        self.offset += buf.len() as u64;
        self.len_seen = len;

        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(&buf);
        let mut lines = Vec::new();
        let mut start = 0;
        let mut skip_first = from_middle;
        for (i, b) in data.iter().enumerate() {
            if *b == b'\n' {
                if !skip_first {
                    push_line(&mut lines, &data[start..i]);
                }
                skip_first = false;
                start = i + 1;
            }
        }
        if skip_first {
            // No newline yet in the window: the whole chunk is a fragment.
            start = data.len();
        }
        self.partial = data[start..].to_vec();
        if self.partial.len() > 32 * 1024 * 1024 {
            // A single line this large is not something we can use.
            self.partial.clear();
        }
        Ok(TailRead { lines, mtime, from_middle, grew })
    }

    /// Streams the decoded content, keeping only what will be emitted, so
    /// even very large compressed rollouts use bounded memory.
    fn read_compressed(&mut self, len: u64, mtime: Millis, initial_window: u64, grew: bool) -> io::Result<TailRead> {
        self.len_seen = len;
        let first = !self.started;
        self.started = true;
        if len > MAX_ZST {
            return Err(io::Error::other("compressed rollout too large"));
        }
        let reader = std::io::BufReader::new(File::open(&self.path)?);
        let mut window: std::collections::VecDeque<Vec<u8>> = std::collections::VecDeque::new();
        let mut window_bytes = 0u64;
        let mut total = 0usize;
        let mut dropped = false;
        let mut fresh = Vec::new();
        for_each_zstd_line(reader, |line| {
            total += 1;
            if first {
                window_bytes += line.len() as u64 + 1;
                window.push_back(line.to_vec());
                while window_bytes > initial_window {
                    if let Some(old) = window.pop_front() {
                        window_bytes -= old.len() as u64 + 1;
                        dropped = true;
                    } else {
                        break;
                    }
                }
            } else if total > self.lines_emitted {
                fresh.push(line.to_vec());
            }
            true
        })?;
        if !first && total < self.lines_emitted {
            // Replaced by a shorter file: start over on the next read.
            self.started = false;
            self.lines_emitted = 0;
            return Ok(TailRead { mtime, grew, ..Default::default() });
        }
        self.lines_emitted = total;
        let mut lines = Vec::new();
        for l in window.iter().chain(fresh.iter()) {
            push_line(&mut lines, l);
        }
        Ok(TailRead { lines, mtime, from_middle: first && dropped, grew })
    }
}

/// Decodes concatenated zstd frames and calls `f` for every complete line.
pub fn for_each_zstd_line(mut reader: impl io::BufRead, mut f: impl FnMut(&[u8]) -> bool) -> io::Result<()> {
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if reader.fill_buf()?.is_empty() {
            break;
        }
        let mut dec = ruzstd::decoding::StreamingDecoder::new(&mut reader)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        loop {
            let n = dec.read(&mut buf)?;
            if n == 0 {
                break;
            }
            carry.extend_from_slice(&buf[..n]);
            let mut start = 0;
            while let Some(pos) = carry[start..].iter().position(|b| *b == b'\n') {
                let line = &carry[start..start + pos];
                if !line.is_empty() && !f(line) {
                    return Ok(());
                }
                start += pos + 1;
            }
            carry.drain(..start);
            if carry.len() > 64 * 1024 * 1024 {
                carry.clear();
            }
        }
    }
    if !carry.is_empty() {
        f(&carry);
    }
    Ok(())
}

fn push_line(out: &mut Vec<String>, raw: &[u8]) {
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    if raw.is_empty() {
        return;
    }
    out.push(String::from_utf8_lossy(raw).into_owned());
}

/// Lists files under `dir` (non-recursive) matching `pred`, with metadata.
pub fn list_files(dir: &Path, pred: impl Fn(&str) -> bool) -> io::Result<Vec<(PathBuf, fs::Metadata)>> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir)?.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !pred(name) {
            continue;
        }
        if let Ok(meta) = e.metadata() {
            if meta.is_file() {
                out.push((e.path(), meta));
            }
        }
    }
    Ok(out)
}

/// Newest modification time of any file below `dir`, bounded in depth and
/// file count so a huge tree cannot stall the monitor.
pub fn newest_mtime(dir: &Path, max_depth: usize, budget: &mut usize) -> Option<Millis> {
    let mut best: Option<Millis> = None;
    let Ok(rd) = fs::read_dir(dir) else { return None };
    for e in rd.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let Ok(meta) = e.metadata() else { continue };
        let t = if meta.is_dir() {
            if max_depth == 0 { None } else { newest_mtime(&e.path(), max_depth - 1, budget) }
        } else {
            Some(mtime_millis(&meta))
        };
        if let Some(t) = t {
            best = Some(best.map_or(t, |b: Millis| b.max(t)));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tails_incrementally_and_handles_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        fs::write(&p, b"{\"a\":1}\n{\"a\":2}\n{\"a\"").unwrap();
        let mut t = FileTail::new(p.clone());
        let r = t.read_new(1 << 20).unwrap();
        assert_eq!(r.lines, vec!["{\"a\":1}", "{\"a\":2}"]);
        assert!(!r.from_middle);
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b":3}\r\n").unwrap();
        drop(f);
        let r = t.read_new(1 << 20).unwrap();
        assert_eq!(r.lines, vec!["{\"a\":3}"]);
        assert!(r.grew);
        let r = t.read_new(1 << 20).unwrap();
        assert!(r.lines.is_empty());
    }

    #[test]
    fn initial_window_skips_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.jsonl");
        let mut content = String::new();
        for i in 0..1000 {
            content.push_str(&format!("{{\"i\":{i}}}\n"));
        }
        fs::write(&p, &content).unwrap();
        let mut t = FileTail::new(p);
        let r = t.read_new(64).unwrap();
        assert!(r.from_middle);
        assert!(!r.lines.is_empty());
        assert_eq!(r.lines.last().unwrap(), "{\"i\":999}");
        for l in &r.lines {
            assert!(serde_json::from_str::<serde_json::Value>(l).is_ok(), "no fragments: {l}");
        }
    }

    #[test]
    fn truncation_resets() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        fs::write(&p, b"{\"a\":1}\n{\"a\":2}\n").unwrap();
        let mut t = FileTail::new(p.clone());
        t.read_new(1 << 20).unwrap();
        fs::write(&p, b"{\"b\":1}\n").unwrap();
        let r = t.read_new(1 << 20).unwrap();
        assert_eq!(r.lines, vec!["{\"b\":1}"]);
    }

    #[test]
    fn compressed_rollouts_stream_with_window_and_increments() {
        use ruzstd::encoding::{CompressionLevel, compress_to_vec};
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("r.jsonl.zst");
        let body: String = (0..500)
            .map(|i| {
                format!(
                    "{{\"i\":{i}}}
"
                )
            })
            .collect();
        // Two concatenated frames, like an appended compressed stream.
        let (a, b) = body.split_at(body.len() / 2);
        let mut raw = compress_to_vec(a.as_bytes(), CompressionLevel::Fastest);
        raw.extend(compress_to_vec(b.as_bytes(), CompressionLevel::Fastest));
        fs::write(&p, &raw).unwrap();
        let mut t = FileTail::new(p.clone());
        let r = t.read_new(100).unwrap();
        assert!(r.from_middle);
        assert_eq!(r.lines.last().unwrap(), "{\"i\":499}");
        assert!(r.lines.len() < 20);
        raw.extend(compress_to_vec(
            "{\"i\":500}
"
            .as_bytes(),
            CompressionLevel::Fastest,
        ));
        fs::write(&p, &raw).unwrap();
        let r = t.read_new(100).unwrap();
        assert_eq!(r.lines, vec!["{\"i\":500}"]);
    }

    #[test]
    fn newest_mtime_respects_budget() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/x.jsonl"), b"1").unwrap();
        let mut budget = 100;
        assert!(newest_mtime(dir.path(), 3, &mut budget).is_some());
        let mut budget = 100;
        assert!(newest_mtime(dir.path(), 0, &mut budget).is_none());
    }
}
