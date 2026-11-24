//! MP4 muxing for packets that are already encoded.
//!
//! The encoder hands us H.264 samples; saving a clip is only a matter of
//! writing a container around them, so saving is disk-speed and never
//! re-encodes. The `mp4` crate writes the moov atom at the end (needs Seek),
//! which is fine for a local file.

use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

use mp4::{AvcConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig};

use crate::config::Codec;
use crate::types::{CodecInfo, Packet};

const VIDEO_TIMESCALE: u32 = 90_000;

pub struct Mp4Muxer {
    writer: Mp4Writer<BufWriter<File>>,
    track_id: u32,
    fps: u32,
    last_dts: i64,
}

impl Mp4Muxer {
    pub fn create(path: &Path, info: &CodecInfo) -> Result<Self, String> {
        if info.codec != Codec::H264 {
            return Err("only H.264 muxing is implemented right now".into());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let file = File::create(path).map_err(|e| e.to_string())?;
        let out = BufWriter::new(file);

        // SPS/PPS live in the AVC config; MF gives them Annex-B framed.
        let (sps, pps) = split_sps_pps(&info.extradata);
        if sps.is_empty() || pps.is_empty() {
            return Err("encoder did not provide SPS/PPS".into());
        }

        let mp4_cfg = Mp4Config {
            major_brand: str::parse("isom").unwrap(),
            minor_version: 512,
            compatible_brands: vec![
                str::parse("isom").unwrap(),
                str::parse("iso2").unwrap(),
                str::parse("avc1").unwrap(),
                str::parse("mp41").unwrap(),
            ],
            timescale: 1000,
        };
        let mut writer = Mp4Writer::write_start(out, &mp4_cfg).map_err(|e| e.to_string())?;

        let track = TrackConfig::from(AvcConfig {
            width: info.width as u16,
            height: info.height as u16,
            seq_param_set: sps,
            pic_param_set: pps,
        });
        // The From<AvcConfig> impl uses a 90 kHz video timescale already, but
        // set it explicitly so timestamps below are unambiguous.
        let track = TrackConfig {
            timescale: VIDEO_TIMESCALE,
            ..track
        };
        writer.add_track(&track).map_err(|e| e.to_string())?;

        Ok(Self {
            writer,
            track_id: 1,
            fps: info.fps.max(1),
            last_dts: -1,
        })
    }

    /// Write one packet. Timestamps are shifted so the first sample starts at 0.
    pub fn write(&mut self, p: &Packet, base_pts_us: i64) -> Result<(), String> {
        let start_us = (p.pts_us - base_pts_us).max(0);
        let start = (start_us as i128 * VIDEO_TIMESCALE as i128 / 1_000_000) as u64;
        let dur = ((p.dur_us.max(1) as i128 * VIDEO_TIMESCALE as i128 + 500_000) / 1_000_000).max(1)
            as u32;

        // Some encoders reorder. Keep decode order monotonic anyway; without
        // B-frames (we ask for none) this is a no-op.
        let start = if (start as i64) <= self.last_dts {
            (self.last_dts + 1) as u64
        } else {
            start
        };
        self.last_dts = start as i64;

        let sample = Mp4Sample {
            start_time: start,
            duration: dur,
            rendering_offset: 0,
            is_sync: p.keyframe,
            bytes: p.data.clone(),
        };
        self.writer
            .write_sample(self.track_id, &sample)
            .map_err(|e| e.to_string())
    }

    pub fn finish(mut self) -> Result<(), String> {
        self.writer.write_end().map_err(|e| e.to_string())?;
        // BufWriter flush happens on drop, but surface IO errors properly.
        let mut inner = self.writer.into_writer();
        inner.flush().map_err(|e| e.to_string())?;
        inner
            .seek(std::io::SeekFrom::End(0))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }
}

/// Split Annex-B extradata into the first SPS and first PPS NALs, start codes
/// removed (that is what avcC wants).
pub fn split_sps_pps(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let nals = annexb_to_nals(data);
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        match nal[0] & 0x1f {
            7 if sps.is_empty() => sps = nal.to_vec(),
            8 if pps.is_empty() => pps = nal.to_vec(),
            _ => {}
        }
    }
    (sps, pps)
}

/// Split an Annex-B byte stream into NAL payloads (no start codes).
pub fn annexb_to_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut start = None;
    while i + 3 <= data.len() {
        let sc3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        if sc3 {
            if let Some(s) = start {
                out.push(&data[s..i]);
            }
            i += 3;
            start = Some(i);
            continue;
        }
        if i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            if let Some(s) = start {
                out.push(&data[s..i]);
            }
            i += 4;
            start = Some(i);
            continue;
        }
        i += 1;
    }
    if let Some(s) = start {
        let mut end = data.len();
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        out.push(&data[s..end]);
    } else {
        // Not Annex-B: assume it is already a single raw NAL.
        out.push(data);
    }
    out
}

/// Normalise a packet to AVCC (4-byte big-endian length prefixes), which is
/// what MP4 sample data must be. If it already looks length-prefixed we leave
/// it alone.
pub fn to_avcc(data: &[u8]) -> Vec<u8> {
    if !starts_with_start_code(data) {
        return data.to_vec();
    }
    let nals = annexb_to_nals(data);
    let total: usize = nals.iter().map(|n| n.len() + 4).sum();
    let mut out = Vec::with_capacity(total);
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

pub fn starts_with_start_code(data: &[u8]) -> bool {
    (data.len() >= 3 && data[0] == 0 && data[1] == 0 && data[2] == 1)
        || (data.len() >= 4 && data[0] == 0 && data[1] == 0 && data[2] == 0 && data[3] == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splices_annexb() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 4];
        let (sps, pps) = split_sps_pps(&data);
        assert_eq!(sps, vec![0x67, 1, 2]);
        assert_eq!(pps, vec![0x68, 3, 4]);
        let avcc = to_avcc(&[0, 0, 1, 0x65, 9, 9]);
        assert_eq!(avcc, vec![0, 0, 0, 3, 0x65, 9, 9]);
    }
}
