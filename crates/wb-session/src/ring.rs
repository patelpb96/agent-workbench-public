//! A terminal's output history: a bounded buffer with absolute stream offsets.
//!
//! Memory is fixed: the buffer holds at most `cap` bytes (1 MiB, or 32 MiB for extended history) plus a little
//! slack so trimming is amortised, and older bytes are dropped (and counted as `skipped` for a late reader).
//! Every update goes through `append`, which cannot panic, so a lock around a `Ring` that ends up poisoned by
//! some OTHER panic still holds a consistent ring; `repair` re-establishes the bounds anyway.
//! Offsets: `base` is the stream offset of `data[0]`; the total stream length is `base + data.len()`.

pub struct Ring {
    data: Vec<u8>,
    base: u64,
    cap: usize,
    slack: usize,
}

impl Ring {
    pub fn new(cap: usize, slack: usize) -> Ring {
        Ring { data: Vec::new(), base: 0, cap: cap.max(1), slack }
    }

    /// Total bytes ever appended (the offset the next byte will have).
    pub fn total(&self) -> u64 {
        self.base.saturating_add(self.data.len() as u64)
    }

    fn trim(&mut self) {
        if self.data.len() > self.cap.saturating_add(self.slack) {
            let cut = self.data.len() - self.cap;
            self.data.drain(..cut);
            self.base = self.base.saturating_add(cut as u64);
        }
    }

    pub fn append(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        self.trim();
    }

    /// Bring the ring back within its bounds (used after a lock was recovered from poison).
    pub fn repair(&mut self) {
        self.trim();
    }

    /// Up to `chunk` bytes from stream offset `from`. Returns (next offset, bytes skipped because they were already
    /// dropped, the bytes). An offset past the end reads nothing and `next` stays at the end.
    pub fn read_from(&self, from: u64, chunk: usize) -> (u64, u64, &[u8]) {
        let total = self.total();
        let (start, skipped) = if from < self.base { (self.base, self.base - from) } else { (from.min(total), 0) };
        let off = usize::try_from(start - self.base).unwrap_or(self.data.len()).min(self.data.len());
        let end = off.saturating_add(chunk).min(self.data.len());
        let slice = self.data.get(off..end).unwrap_or(&[]);
        (start.saturating_add(slice.len() as u64), skipped, slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_follow_the_stream_offsets() {
        let mut r = Ring::new(100, 10);
        assert_eq!(r.read_from(0, 10), (0, 0, &b""[..]), "empty ring");
        r.append(b"hello world");
        assert_eq!(r.total(), 11);
        assert_eq!(r.read_from(0, 5), (5, 0, &b"hello"[..]));
        assert_eq!(r.read_from(5, 100), (11, 0, &b" world"[..]));
        assert_eq!(r.read_from(11, 100), (11, 0, &b""[..]), "caught up");
        assert_eq!(r.read_from(500, 100), (11, 0, &b""[..]), "an offset past the end clamps to the end");
        assert_eq!(r.read_from(3, 0), (3, 0, &b""[..]), "a zero-size chunk reads nothing");
    }

    #[test]
    fn the_buffer_stays_bounded_and_a_late_reader_is_told_what_it_missed() {
        let mut r = Ring::new(100, 20);
        for i in 0..1000u32 {
            r.append(format!("{i:04}").as_bytes()); // 4 bytes at a time
            assert!(r.data.len() <= 120, "never above cap + slack");
        }
        assert_eq!(r.total(), 4000);
        let (next, skipped, bytes) = r.read_from(0, 1 << 20);
        assert!((3880..=3900).contains(&skipped), "skipped {skipped}");
        assert_eq!(next, 4000);
        assert_eq!(bytes.len() as u64 + skipped, 4000, "skipped plus delivered is the whole stream");
        assert!(bytes.ends_with(b"0999"));
    }

    #[test]
    fn a_huge_single_append_is_trimmed_to_the_cap() {
        let mut r = Ring::new(64, 8);
        r.append(&vec![b'x'; 10_000]);
        assert_eq!(r.data.len(), 64);
        assert_eq!(r.total(), 10_000);
        assert_eq!(r.read_from(0, 1000).1, 10_000 - 64);
    }

    #[test]
    fn repair_restores_the_bounds_and_keeps_the_offsets_consistent() {
        let mut r = Ring::new(50, 10);
        r.data = vec![b'z'; 500]; // as if an update was cut short
        r.repair();
        assert_eq!(r.data.len(), 50);
        assert_eq!(r.total(), 500, "the stream offsets still add up");
        let before = (r.data.clone(), r.base);
        r.repair();
        assert_eq!((r.data.clone(), r.base), before, "repairing a healthy ring changes nothing");
    }

    #[test]
    fn offsets_near_the_numeric_limits_do_not_panic() {
        let mut r = Ring::new(16, 4);
        r.append(b"abc");
        for from in [0, 1, u64::MAX, u64::MAX - 1, 1 << 63] {
            for chunk in [0, 1, usize::MAX] {
                let _ = r.read_from(from, chunk);
            }
        }
    }
}
