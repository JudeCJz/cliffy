//! Ring buffer of *encoded* packets.
//!
//! This is the reason the replay buffer is cheap: we never keep decoded frames.
//! At 20 Mbps a 60 second buffer is roughly 150 MB, and that memory is what the
//! spec excludes from the app's footprint.

use std::collections::VecDeque;

use crate::config::REPLAY_RAM_CAP_BYTES;
use crate::types::Packet;

pub struct PacketRing {
    packets: VecDeque<Packet>,
    max_duration_us: i64,
    max_bytes: u64,
    bytes: u64,
}

impl PacketRing {
    pub fn new(max_duration_secs: u32) -> Self {
        Self {
            packets: VecDeque::new(),
            max_duration_us: max_duration_secs as i64 * 1_000_000,
            // Safety ceiling so a pathological bitrate can never eat all RAM.
            max_bytes: REPLAY_RAM_CAP_BYTES,
            bytes: 0,
        }
    }

    pub fn set_duration(&mut self, secs: u32) {
        self.max_duration_us = secs as i64 * 1_000_000;
        self.trim();
    }

    pub fn push(&mut self, p: Packet) {
        self.bytes += p.data.len() as u64;
        self.packets.push_back(p);
        self.trim();
    }

    /// Drop old packets, but never split a GOP: keep the newest keyframe at or
    /// before the window start, so a saved clip can always begin decoding.
    fn trim(&mut self) {
        let latest = match self.packets.back() {
            Some(p) => p.end_us(),
            None => return,
        };
        let cutoff = latest - self.max_duration_us;

        // Newest keyframe at or before the cutoff. Scan forward so the last
        // matching index wins; scanning back would keep the oldest keyframe.
        let mut cut = 0usize;
        for (i, p) in self.packets.iter().enumerate() {
            if p.pts_us <= cutoff && p.keyframe {
                cut = i;
            }
        }
        for _ in 0..cut {
            if let Some(p) = self.packets.pop_front() {
                self.bytes -= p.data.len() as u64;
            }
        }

        // Emergency ceiling only; may break the GOP rule but never eats RAM.
        while self.bytes > self.max_bytes && self.packets.len() > 1 {
            if let Some(p) = self.packets.pop_front() {
                self.bytes -= p.data.len() as u64;
            }
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    pub fn duration_us(&self) -> i64 {
        match (self.packets.front(), self.packets.back()) {
            (Some(f), Some(b)) => (b.end_us() - f.pts_us).max(0),
            _ => 0,
        }
    }

    /// Packets covering the last `seconds`, starting at a keyframe.
    pub fn snapshot_last(&self, seconds: u32) -> Vec<Packet> {
        if self.packets.is_empty() {
            return Vec::new();
        }
        let latest = self.packets.back().map(|p| p.end_us()).unwrap_or(0);
        let cutoff = latest - seconds as i64 * 1_000_000;

        // Find the last keyframe at or before the cutoff; fall back to the
        // oldest keyframe available if the buffer is shorter than requested.
        let mut start = None;
        for (i, p) in self.packets.iter().enumerate() {
            if p.keyframe && p.pts_us <= cutoff {
                start = Some(i);
            }
        }
        let start =
            start.unwrap_or_else(|| self.packets.iter().position(|p| p.keyframe).unwrap_or(0));

        self.packets.iter().skip(start).cloned().collect()
    }

    pub fn clear(&mut self) {
        self.packets.clear();
        self.bytes = 0;
    }

    pub fn len(&self) -> usize {
        self.packets.len()
    }
}
