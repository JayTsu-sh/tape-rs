//! Limits for provisional streaming XML: bound an event before quick-xml can
//! grow its buffer, and bound metadata retained by the current open nodes.
use std::io::{self, BufRead, Read};

use crate::error::{Result, TapeError};

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub event: usize,
    pub field: usize,
    pub resident: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            event: 64 << 20,
            field: 64 << 20,
            resident: 128 << 20,
        }
    }
}
fn exceeded() -> TapeError {
    TapeError::LocalCapacity {
        reason: "流式索引 XML 事件、字段或当前节点元数据超过内存预算".into(),
    }
}

pub(super) struct EventReader<R> {
    inner: R,
    limit: Option<usize>,
    remaining: usize,
    exhausted: bool,
}
impl<R> EventReader<R> {
    pub fn new(inner: R, limit: Option<usize>) -> Self {
        Self {
            inner,
            limit,
            remaining: limit.unwrap_or(usize::MAX),
            exhausted: false,
        }
    }
    pub fn next_event(&mut self) {
        self.remaining = self.limit.unwrap_or(usize::MAX);
    }
    pub fn exhausted(&self) -> bool {
        self.exhausted
    }
}
impl<R: BufRead> Read for EventReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let data = self.fill_buf()?;
        let n = data.len().min(out.len());
        out[..n].copy_from_slice(&data[..n]);
        self.consume(n);
        Ok(n)
    }
}
impl<R: BufRead> BufRead for EventReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.remaining == 0 {
            self.exhausted = true;
            return Err(io::Error::other("streamed XML event budget exceeded"));
        }
        let data = self.inner.fill_buf()?;
        Ok(&data[..data.len().min(self.remaining)])
    }
    fn consume(&mut self, n: usize) {
        self.remaining = self.remaining.saturating_sub(n);
        self.inner.consume(n);
    }
}

pub(super) struct MetadataBudget {
    pub limits: Limits,
    nodes: Vec<usize>,
    resident: usize,
    header: usize,
}
impl MetadataBudget {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            nodes: Vec::new(),
            resident: 0,
            header: 0,
        }
    }
    pub fn start(&mut self, node: bool, bytes: usize) -> Result<()> {
        if node {
            self.nodes.push(0);
        }
        self.charge(bytes.checked_add(64).ok_or_else(exceeded)?)
    }
    pub fn charge(&mut self, bytes: usize) -> Result<()> {
        if let Some(node) = self.nodes.last_mut() {
            let next = self
                .resident
                .checked_add(bytes)
                .filter(|n| *n <= self.limits.resident)
                .ok_or_else(exceeded)?;
            *node = node.checked_add(bytes).ok_or_else(exceeded)?;
            self.resident = next;
        } else {
            self.header = self
                .header
                .checked_add(bytes)
                .filter(|n| *n <= self.limits.resident)
                .ok_or_else(exceeded)?;
        }
        Ok(())
    }
    pub fn end_node(&mut self) -> Result<()> {
        let bytes = self.nodes.pop().ok_or_else(exceeded)?;
        self.resident = self.resident.checked_sub(bytes).ok_or_else(exceeded)?;
        Ok(())
    }
    pub fn text(&self, existing: usize, added: usize) -> Result<()> {
        if existing
            .checked_add(added)
            .is_none_or(|n| n > self.limits.field)
        {
            return Err(exceeded());
        }
        Ok(())
    }
}

pub(super) fn event_exceeded() -> TapeError {
    exceeded()
}
