//! Incremental, read-only tailing of a growing JSONL transcript by byte offset.
use crate::{
    adapters::{Adapter, ClaudeAdapter, CodexAdapter, PiAdapter},
    index::Index,
};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::PathBuf,
};

pub const MAX_LINE: usize = 8 << 20;
const CHUNK: usize = 1 << 20;
/// Per poll, so one large backlog cannot monopolise the broker.
pub const MAX_BYTES_PER_POLL: usize = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Pi,
    Codex,
}
impl AgentKind {
    pub fn adapter(self) -> Box<dyn Adapter + Send> {
        match self {
            Self::Claude => Box::new(ClaudeAdapter::new()),
            Self::Pi => Box::new(PiAdapter::new()),
            Self::Codex => Box::new(CodexAdapter::new()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PollResult {
    pub bytes: usize,
    pub lines: u64,
    pub events: u64,
    /// The file shrank or was replaced: state was rebuilt from the start.
    pub reset: bool,
    pub more: bool,
}

pub struct Tailer {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    skipping: bool,
    pub oversized_lines: u64,
}

impl Tailer {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), offset: 0, partial: Vec::new(), skipping: false, oversized_lines: 0 }
    }
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Read newly appended complete lines into `adapter` and `index`. Returns
    /// `reset = true` (having consumed nothing) when the file shrank; the caller must
    /// then rebuild its adapter and index and poll again.
    pub fn poll(&mut self, adapter: &mut dyn Adapter, index: &mut Index) -> io::Result<PollResult> {
        let mut file = File::open(&self.path)?;
        let len = file.metadata()?.len();
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
            self.skipping = false;
            return Ok(PollResult { reset: true, ..Default::default() });
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut out = PollResult::default();
        let mut buf = vec![0u8; CHUNK];
        while out.bytes < MAX_BYTES_PER_POLL {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.bytes += n;
            self.offset += n as u64;
            for &b in &buf[..n] {
                if b == b'\n' {
                    if self.skipping {
                        self.skipping = false;
                    } else {
                        if self.partial.last() == Some(&b'\r') {
                            self.partial.pop();
                        }
                        out.lines += 1;
                        if let Ok(line) = std::str::from_utf8(&self.partial) {
                            for e in adapter.parse_line(line) {
                                index.apply(&e);
                                out.events += 1;
                            }
                        }
                    }
                    self.partial.clear();
                } else if !self.skipping {
                    if self.partial.len() >= MAX_LINE {
                        // Drop the oversized line entirely; resume at the next newline.
                        self.partial.clear();
                        self.skipping = true;
                        self.oversized_lines += 1;
                    } else {
                        self.partial.push(b);
                    }
                }
            }
        }
        out.more = out.bytes >= MAX_BYTES_PER_POLL;
        Ok(out)
    }
}

/// Convenience bundle: tail one transcript into one index, rebuilding on reset.
pub struct Observer {
    kind: AgentKind,
    tailer: Tailer,
    adapter: Box<dyn Adapter + Send>,
    pub index: Index,
}
impl Observer {
    pub fn new(kind: AgentKind, path: impl Into<PathBuf>) -> Self {
        Self { kind, tailer: Tailer::new(path), adapter: kind.adapter(), index: Index::new() }
    }
    pub fn poll(&mut self) -> io::Result<PollResult> {
        let r = self.tailer.poll(self.adapter.as_mut(), &mut self.index)?;
        if r.reset {
            self.adapter = self.kind.adapter();
            self.index = Index::new();
            return self.tailer.poll(self.adapter.as_mut(), &mut self.index).map(|mut r2| {
                r2.reset = true;
                r2
            });
        }
        Ok(r)
    }
    pub fn tailer(&self) -> &Tailer {
        &self.tailer
    }
}
