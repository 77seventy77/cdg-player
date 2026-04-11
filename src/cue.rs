/// Minimal .cue sheet parser for redump.org multi-bin style sheets.
///
/// Layout: one .bin file per track, all AUDIO.  The companion .cdg is a
/// flat R-W subcode dump of the entire disc (4 packets × 24 bytes = 96
/// bytes per sector).  Sector numbers are absolute from the start of the
/// first bin.
use std::path::{Path, PathBuf};

const SECTOR_BYTES: usize = 2352;
const CDG_BYTES_PER_SECTOR: usize = 96; // 4 packets × 24 bytes

pub const SAMPLE_RATE: u32 = 44100;
pub const CHANNELS: u16 = 2;

#[derive(Debug, Clone)]
pub struct Track {
    pub number: u32,
    pub bin_path: PathBuf,
    /// Byte offset within the .bin file where INDEX 01 audio begins.
    pub bin_audio_offset: u64,
    /// Absolute disc sector at INDEX 01 (used to seek in .cdg).
    pub abs_sector: u64,
    /// Length of this track in sectors (computed after all tracks are parsed).
    pub sectors: u64,
}

impl Track {
    /// Byte offset into the .cdg file for this track's start.
    pub fn cdg_offset(&self) -> u64 {
        self.abs_sector * CDG_BYTES_PER_SECTOR as u64
    }

    /// Load this track's audio as interleaved i16 samples (L, R, …).
    pub fn load_audio(&self) -> Vec<i16> {
        let data = std::fs::read(&self.bin_path).unwrap_or_else(|e| {
            eprintln!("Cannot read {:?}: {e}", self.bin_path);
            std::process::exit(1);
        });
        let start = self.bin_audio_offset as usize;
        let end = (start + self.sectors as usize * SECTOR_BYTES).min(data.len());
        if start >= data.len() {
            return Vec::new();
        }
        let audio_bytes = &data[start..end];
        let mut samples = Vec::with_capacity(audio_bytes.len() / 2);
        for chunk in audio_bytes.chunks_exact(2) {
            samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }
        samples
    }
}

/// Parse a .cue sheet and return all audio tracks with absolute sector info.
pub fn parse_cue(cue_path: &Path) -> Vec<Track> {
    let text = std::fs::read_to_string(cue_path).unwrap_or_else(|e| {
        eprintln!("Cannot read cue file {:?}: {e}", cue_path);
        std::process::exit(1);
    });
    let cue_dir = cue_path.parent().unwrap_or(Path::new("."));

    // --- first pass: collect raw entries ----
    struct RawTrack {
        number: u32,
        bin_path: PathBuf,
        bin_size_sectors: u64,
        index01_within_bin: u64, // sector offset of INDEX 01 inside this bin
        is_audio: bool,
    }

    let mut raw: Vec<RawTrack> = Vec::new();
    let mut cur_bin: Option<(PathBuf, u64)> = None; // (path, size_in_sectors)
    let mut cur_number = 0u32;
    let mut cur_is_audio = false;
    let mut cur_index01: Option<u64> = None;

    let flush = |raw: &mut Vec<RawTrack>,
                 cur_bin: &Option<(PathBuf, u64)>,
                 cur_number: u32,
                 cur_is_audio: bool,
                 cur_index01: Option<u64>| {
        if cur_number == 0 { return; }
        let Some((ref path, size)) = *cur_bin else { return };
        raw.push(RawTrack {
            number: cur_number,
            bin_path: path.clone(),
            bin_size_sectors: size,
            index01_within_bin: cur_index01.unwrap_or(0),
            is_audio: cur_is_audio,
        });
    };

    for line in text.lines() {
        let trimmed = line.trim();
        let upper = trimmed.to_uppercase();

        if upper.starts_with("FILE ") {
            flush(&mut raw, &cur_bin, cur_number, cur_is_audio, cur_index01);
            cur_number = 0;
            cur_is_audio = false;
            cur_index01 = None;

            if let Some(name) = extract_quoted(trimmed) {
                let path = cue_dir.join(name);
                let size = std::fs::metadata(&path)
                    .map(|m| m.len() / SECTOR_BYTES as u64)
                    .unwrap_or(0);
                cur_bin = Some((path, size));
            }
        } else if upper.starts_with("TRACK ") {
            if cur_number != 0 {
                flush(&mut raw, &cur_bin, cur_number, cur_is_audio, cur_index01);
            }
            cur_is_audio = upper.contains("AUDIO");
            cur_index01 = None;
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            cur_number = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        } else if let Some(rest) = upper.strip_prefix("INDEX ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Some(sectors) = msf_to_sectors(parts[1]) {
                    match parts[0] {
                        "01" => cur_index01 = Some(sectors),
                        _ => {}
                    }
                }
            }
        }
    }
    flush(&mut raw, &cur_bin, cur_number, cur_is_audio, cur_index01);

    // --- second pass: compute absolute sectors and lengths ----------------
    let mut tracks: Vec<Track> = Vec::new();
    let mut abs_sector_cursor: u64 = 0;

    for (i, r) in raw.iter().enumerate() {
        if !r.is_audio { continue; }

        let abs_index01 = abs_sector_cursor + r.index01_within_bin;

        // Length = remaining sectors in this bin from INDEX 01
        let sectors = r.bin_size_sectors.saturating_sub(r.index01_within_bin);

        tracks.push(Track {
            number: r.number,
            bin_path: r.bin_path.clone(),
            bin_audio_offset: r.index01_within_bin * SECTOR_BYTES as u64,
            abs_sector: abs_index01,
            sectors,
        });

        abs_sector_cursor += r.bin_size_sectors;
        let _ = i;
    }

    tracks
}

fn msf_to_sectors(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 { return None; }
    let m: u64 = parts[0].parse().ok()?;
    let s2: u64 = parts[1].parse().ok()?;
    let f: u64 = parts[2].parse().ok()?;
    Some((m * 60 + s2) * 75 + f)
}

fn extract_quoted(line: &str) -> Option<&str> {
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    Some(&line[start..end])
}
