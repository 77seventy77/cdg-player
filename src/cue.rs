/// Minimal .cue sheet parser for redump.org multi-bin style sheets.
///
/// Supports two bin sources:
///   - Filesystem paths (regular discs, extracted archives).
///   - STORE-only ZIP entries accessed by byte offset (no extraction needed).
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const SECTOR_BYTES: usize = 2352;
pub const CDG_BYTES_PER_SECTOR: usize = 96; // 4 packets × 24 bytes

pub const SAMPLE_RATE: u32 = 44100;
pub const CHANNELS: u16 = 2;

// ── ZipEntry ──────────────────────────────────────────────────────────────────

/// Reference to uncompressed data inside a STORE-only ZIP file.
/// Because the data is not compressed, it sits at a fixed byte offset and can
/// be read by opening the ZIP as a plain file and seeking to `data_start`.
#[derive(Debug, Clone)]
pub struct ZipEntry {
    pub zip_path: PathBuf,
    /// Byte offset of the raw (uncompressed) data within the ZIP file.
    pub data_start: u64,
    /// Number of bytes of data.
    pub data_size: u64,
}

impl ZipEntry {
    pub fn read_all(&self) -> Vec<u8> {
        self.read_slice(0, self.data_size as usize)
    }

    /// Read `len` bytes starting `offset` bytes into this entry's data.
    pub fn read_slice(&self, offset: u64, len: usize) -> Vec<u8> {
        let Ok(mut f) = std::fs::File::open(&self.zip_path) else {
            return vec![];
        };
        if f.seek(SeekFrom::Start(self.data_start + offset)).is_err() {
            return vec![];
        }
        let cap = len.min(self.data_size.saturating_sub(offset) as usize);
        let mut buf = vec![0u8; cap];
        let _ = f.read_exact(&mut buf);
        buf
    }
}

// ── Track ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Track {
    pub number: u32,
    /// On-disk .bin path.  None for ZIP-backed tracks.
    pub bin_path: Option<PathBuf>,
    /// ZIP source for the .bin data.  Takes priority over `bin_path`.
    pub bin_zip: Option<ZipEntry>,
    /// Byte offset within the .bin data where INDEX 01 audio begins.
    pub bin_audio_offset: u64,
    /// Absolute disc sector at INDEX 01 (used to seek in a monolithic .cdg).
    pub abs_sector: u64,
    /// Length of this track in sectors.
    pub sectors: u64,
    /// Per-track on-disk CDG file (the whole file is this track's data).
    pub cdg_path: Option<PathBuf>,
    /// Per-track CDG entry inside a STORE ZIP.  Takes priority over `cdg_path`.
    pub cdg_zip: Option<ZipEntry>,
}

impl Track {
    /// Byte offset into a monolithic .cdg file for this track's start.
    pub fn cdg_offset(&self) -> u64 {
        self.abs_sector * CDG_BYTES_PER_SECTOR as u64
    }

    /// Read this track's CDG bytes from whichever source is available.
    ///
    /// Priority: per-track ZIP → per-track file → monolithic ZIP (sliced) →
    /// monolithic file bytes (pre-loaded, sliced).
    pub fn read_cdg(
        &self,
        global_raw: Option<&[u8]>,
        global_zip: Option<&ZipEntry>,
    ) -> Vec<u8> {
        if let Some(ref z) = self.cdg_zip {
            return z.read_all();
        }
        if let Some(ref p) = self.cdg_path {
            return std::fs::read(p).unwrap_or_default();
        }
        if let Some(z) = global_zip {
            let offset = self.cdg_offset();
            let len = self.sectors as usize * CDG_BYTES_PER_SECTOR;
            return z.read_slice(offset, len);
        }
        if let Some(raw) = global_raw {
            let start = self.cdg_offset() as usize;
            let end = (start + self.sectors as usize * CDG_BYTES_PER_SECTOR).min(raw.len());
            return raw[start.min(raw.len())..end].to_vec();
        }
        vec![]
    }

    /// Returns true if this track has any CDG source (per-track or implied by
    /// global sources passed by the caller).
    pub fn has_cdg(&self, has_global: bool) -> bool {
        self.cdg_zip.is_some() || self.cdg_path.is_some() || has_global
    }

    /// Load this track's audio as interleaved i16 samples (L, R, …).
    pub fn load_audio(&self) -> Vec<i16> {
        let audio_bytes = if let Some(ref z) = self.bin_zip {
            // Read only the audio portion directly from the ZIP.
            let audio_len = (self.sectors as usize * SECTOR_BYTES)
                .min(z.data_size.saturating_sub(self.bin_audio_offset) as usize);
            z.read_slice(self.bin_audio_offset, audio_len)
        } else if let Some(ref path) = self.bin_path {
            let data = match std::fs::read(path) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("Cannot read {:?}: {e}", path);
                    return Vec::new();
                }
            };
            let start = self.bin_audio_offset as usize;
            let end = (start + self.sectors as usize * SECTOR_BYTES).min(data.len());
            if start >= data.len() {
                return Vec::new();
            }
            data[start..end].to_vec()
        } else {
            return Vec::new();
        };

        let mut samples = Vec::with_capacity(audio_bytes.len() / 2);
        for chunk in audio_bytes.chunks_exact(2) {
            samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }
        samples
    }
}

// ── Shared CUE parser ─────────────────────────────────────────────────────────

/// What a FILE resolution step returns.
#[derive(Clone)]
struct ResolvedBin {
    bin_path: Option<PathBuf>,
    bin_zip: Option<ZipEntry>,
    size_sectors: u64,
    cdg_path: Option<PathBuf>,
    cdg_zip: Option<ZipEntry>,
}

/// Core CUE parser.  `resolve_bin` maps a FILE name (as written in the .cue)
/// to the bin's source and size.  Returns None to skip unresolvable entries.
fn parse_cue_impl(text: &str, mut resolve_bin: impl FnMut(&str) -> Option<ResolvedBin>) -> Vec<Track> {
    struct RawTrack {
        number: u32,
        resolved: ResolvedBin,
        size_sectors: u64,
        index01_within_bin: u64,
        is_audio: bool,
    }

    let mut raw: Vec<RawTrack> = Vec::new();
    let mut cur_resolved: Option<ResolvedBin> = None;
    let mut cur_size_sectors: u64 = 0;
    let mut cur_number = 0u32;
    let mut cur_is_audio = false;
    let mut cur_index01: Option<u64> = None;

    macro_rules! flush {
        () => {
            if cur_number != 0 {
                if let Some(resolved) = cur_resolved.take() {
                    raw.push(RawTrack {
                        number: cur_number,
                        resolved,
                        size_sectors: cur_size_sectors,
                        index01_within_bin: cur_index01.unwrap_or(0),
                        is_audio: cur_is_audio,
                    });
                }
                cur_number = 0; // reset so a subsequent FILE block starts fresh
            }
        };
    }

    for line in text.lines() {
        let trimmed = line.trim();
        let upper = trimmed.to_uppercase();

        if upper.starts_with("FILE ") {
            flush!();
            cur_is_audio = false;
            cur_index01 = None;

            if let Some(name) = extract_quoted(trimmed) {
                if let Some(r) = resolve_bin(name) {
                    cur_size_sectors = r.size_sectors;
                    cur_resolved = Some(r);
                } else {
                    cur_resolved = None;
                    cur_size_sectors = 0;
                }
            }
        } else if upper.starts_with("TRACK ") {
            if cur_number != 0 {
                if let Some(ref resolved) = cur_resolved {
                    raw.push(RawTrack {
                        number: cur_number,
                        resolved: resolved.clone(),
                        size_sectors: cur_size_sectors,
                        index01_within_bin: cur_index01.unwrap_or(0),
                        is_audio: cur_is_audio,
                    });
                }
            }
            cur_is_audio = upper.contains("AUDIO");
            cur_index01 = None;
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            cur_number = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        } else if let Some(rest) = upper.strip_prefix("INDEX ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Some(sectors) = msf_to_sectors(parts[1]) {
                    if parts[0] == "01" {
                        cur_index01 = Some(sectors);
                    }
                }
            }
        }
    }
    flush!();

    // Second pass: compute absolute sectors and lengths.
    let mut tracks: Vec<Track> = Vec::new();
    let mut abs_cursor: u64 = 0;

    for r in raw {
        if !r.is_audio {
            abs_cursor += r.size_sectors;
            continue;
        }

        let abs_index01 = abs_cursor + r.index01_within_bin;
        let sectors = r.size_sectors.saturating_sub(r.index01_within_bin);

        tracks.push(Track {
            number: r.number,
            bin_path: r.resolved.bin_path,
            bin_zip: r.resolved.bin_zip,
            bin_audio_offset: r.index01_within_bin * SECTOR_BYTES as u64,
            abs_sector: abs_index01,
            sectors,
            cdg_path: r.resolved.cdg_path,
            cdg_zip: r.resolved.cdg_zip,
        });

        abs_cursor += r.size_sectors;
    }

    tracks
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Parse a .cue sheet from disk, resolving FILE references against the
/// filesystem (existing behaviour).
pub fn parse_cue(cue_path: &Path) -> Vec<Track> {
    let text = match std::fs::read_to_string(cue_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Cannot read cue file {:?}: {e}", cue_path);
            return Vec::new();
        }
    };
    let cue_dir = cue_path.parent().unwrap_or(Path::new("."));

    parse_cue_impl(&text, |name| {
        let resolved = resolve_fs_bin(name, cue_dir);
        let size = std::fs::metadata(&resolved)
            .map(|m| m.len() / SECTOR_BYTES as u64)
            .unwrap_or(0);
        let cdg_candidate = resolved.with_extension("cdg");
        let cdg_path = if cdg_candidate.exists()
            && cdg_candidate
                .metadata()
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        {
            Some(cdg_candidate)
        } else {
            None
        };
        Some(ResolvedBin {
            bin_path: Some(resolved),
            bin_zip: None,
            size_sectors: size,
            cdg_path,
            cdg_zip: None,
        })
    })
}

/// Parse a .cue sheet whose FILE entries reside in a STORE ZIP.
/// `entry_map` maps ASCII-folded filenames to their ZipEntry (call-site
/// already scanned the archive).
pub fn parse_cue_from_zip(
    cue_text: &str,
    zip_path: &Path,
    entry_map: &HashMap<String, (u64, u64)>, // ascii-folded name → (data_start, data_size)
) -> Vec<Track> {
    parse_cue_impl(cue_text, |name| {
        let key = ascii_fold(name);

        // Pass 1: exact ASCII-fold match.
        let bin_info = entry_map.get(&key).copied().or_else(|| {
            // Pass 2: match by track number.
            let n = track_num_from_name(name)?;
            entry_map.iter().find(|(k, _)| {
                k.ends_with(".bin") && track_num_from_name(k) == Some(n)
            }).map(|(_, v)| *v)
        });

        let (bin_start, bin_size) = bin_info?;

        // Per-track CDG: replace ".bin" with ".cdg" in the ASCII-folded key.
        let cdg_key = if key.ends_with(".bin") {
            format!("{}.cdg", &key[..key.len() - 4])
        } else {
            key.clone()
        };
        let cdg_zip = entry_map.get(&cdg_key).map(|&(ds, sz)| ZipEntry {
            zip_path: zip_path.to_path_buf(),
            data_start: ds,
            data_size: sz,
        });

        Some(ResolvedBin {
            bin_path: None,
            bin_zip: Some(ZipEntry {
                zip_path: zip_path.to_path_buf(),
                data_start: bin_start,
                data_size: bin_size,
            }),
            size_sectors: bin_size / SECTOR_BYTES as u64,
            cdg_path: None,
            cdg_zip,
        })
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolve a FILE reference to an on-disk path, applying fuzzy matching for
/// Unicode encoding/normalization mismatches.
fn resolve_fs_bin(name: &str, cue_dir: &Path) -> PathBuf {
    let path = cue_dir.join(name);
    if path.exists() {
        return path;
    }

    let bins: Vec<_> = std::fs::read_dir(cue_dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.path()
                        .extension()
                        .and_then(|x| x.to_str())
                        .map_or(false, |x| x.eq_ignore_ascii_case("bin"))
                })
                .collect()
        })
        .unwrap_or_default();

    let ascii_key = ascii_fold(name);
    let matched = bins
        .iter()
        .find(|e| ascii_fold(&e.file_name().to_string_lossy()) == ascii_key);

    let matched = matched.or_else(|| {
        let n = track_num_from_name(name)?;
        bins.iter()
            .find(|e| track_num_from_name(&e.file_name().to_string_lossy()) == Some(n))
    });

    matched.map(|e| e.path()).unwrap_or(path)
}

fn msf_to_sectors(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
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

/// Lowercase and keep only ASCII characters.
pub fn ascii_fold(s: &str) -> String {
    s.chars()
        .filter_map(|c| {
            if c.is_ascii() {
                Some(c.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect()
}

/// Extract the track number from a filename containing "(Track N)" or "Track N".
fn track_num_from_name(s: &str) -> Option<u32> {
    let lower = s.to_lowercase();
    let pos = lower.find("track")?;
    let rest = lower[pos + 5..].trim_start_matches(|c: char| !c.is_ascii_digit());
    rest.chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}
