//! Sorted, coalesced set of half-open byte ranges `[start, end)`.
//!
//! Used to track which parts of a file are present in the local cache, so a
//! 10 GB video can be streamed without downloading it whole.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RangeSet(Vec<(u64, u64)>);

impl RangeSet {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// A set covering `[0, size)`.
    pub fn full(size: u64) -> Self {
        if size == 0 {
            Self::new()
        } else {
            Self(vec![(0, size)])
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Total number of bytes covered.
    pub fn len(&self) -> u64 {
        self.0.iter().map(|(s, e)| e - s).sum()
    }

    /// Highest covered offset (exclusive), 0 when empty.
    pub fn max_end(&self) -> u64 {
        self.0.last().map_or(0, |r| r.1)
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Adds `[start, end)` and returns how many bytes were not covered before.
    pub fn insert(&mut self, start: u64, end: u64) -> u64 {
        if start >= end {
            return 0;
        }
        let before = self.len();
        let (mut ns, mut ne) = (start, end);
        let mut out = Vec::with_capacity(self.0.len() + 1);
        let mut placed = false;
        for &(s, e) in &self.0 {
            if e < ns {
                out.push((s, e));
            } else if s > ne {
                if !placed {
                    out.push((ns, ne));
                    placed = true;
                }
                out.push((s, e));
            } else {
                // Overlapping or adjacent: merge into the new range.
                ns = ns.min(s);
                ne = ne.max(e);
            }
        }
        if !placed {
            out.push((ns, ne));
        }
        self.0 = out;
        self.len() - before
    }

    /// True when every byte of `[start, end)` is covered (an empty range always is).
    pub fn contains(&self, start: u64, end: u64) -> bool {
        start >= end || self.0.iter().any(|&(s, e)| s <= start && end <= e)
    }

    /// True when `[0, size)` is fully covered.
    pub fn is_complete(&self, size: u64) -> bool {
        self.contains(0, size)
    }

    /// First offset inside `[start, end)` that is not covered.
    pub fn first_gap(&self, start: u64, end: u64) -> Option<u64> {
        let mut pos = start;
        for &(s, e) in &self.0 {
            if e <= pos {
                continue;
            }
            if s > pos {
                break;
            }
            pos = e;
            if pos >= end {
                return None;
            }
        }
        (pos < end).then_some(pos)
    }

    /// Start of the first covered range that begins strictly after `pos`.
    pub fn next_start_after(&self, pos: u64) -> Option<u64> {
        self.0.iter().map(|r| r.0).find(|&s| s > pos)
    }

    /// Drops everything at or beyond `size`; returns the number of bytes removed.
    pub fn truncate(&mut self, size: u64) -> u64 {
        let before = self.len();
        self.0.retain_mut(|r| {
            r.1 = r.1.min(size);
            r.0 < r.1
        });
        before - self.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_merges_overlapping_and_adjacent() {
        let mut r = RangeSet::new();
        assert_eq!(r.insert(10, 20), 10);
        assert_eq!(r.insert(30, 40), 10);
        assert_eq!(r.insert(20, 30), 10); // bridges both
        assert_eq!(r, RangeSet(vec![(10, 40)]));
        assert_eq!(r.insert(0, 15), 10);
        assert_eq!(r.insert(5, 12), 0);
        assert_eq!(r.len(), 40);
    }

    #[test]
    fn contains_and_gaps() {
        let mut r = RangeSet::new();
        r.insert(0, 100);
        r.insert(200, 300);
        assert!(r.contains(0, 100));
        assert!(r.contains(50, 50));
        assert!(!r.contains(50, 150));
        assert!(!r.is_complete(300));
        assert_eq!(r.first_gap(0, 300), Some(100));
        assert_eq!(r.first_gap(150, 300), Some(150));
        assert_eq!(r.first_gap(200, 300), None);
        assert_eq!(r.first_gap(250, 400), Some(300));
        assert_eq!(r.next_start_after(0), Some(200));
        assert_eq!(r.next_start_after(200), None);
        r.insert(100, 200);
        assert!(r.is_complete(300));
    }

    #[test]
    fn truncate_and_full() {
        let mut r = RangeSet::full(1000);
        assert_eq!(r.truncate(400), 600);
        assert_eq!(r.max_end(), 400);
        let mut r = RangeSet::new();
        r.insert(0, 10);
        r.insert(20, 30);
        assert_eq!(r.truncate(20), 10);
        assert_eq!(r, RangeSet(vec![(0, 10)]));
        assert!(RangeSet::full(0).is_complete(0));
    }
}
