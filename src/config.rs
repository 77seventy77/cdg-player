use std::path::{Path, PathBuf};

fn config_path() -> PathBuf {
    let base = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("cdg-player").join("config")
}

pub struct Config {
    pub library_path: Option<PathBuf>,
}

impl Config {
    pub fn load() -> Self {
        let library_path = std::fs::read_to_string(config_path())
            .ok()
            .map(|s| PathBuf::from(s.trim()))
            .filter(|p| p.exists());
        Config { library_path }
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Some(ref lib) = self.library_path {
            let _ = std::fs::write(&path, lib.to_string_lossy().as_ref());
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }

    pub fn set_library(&mut self, path: PathBuf) {
        self.library_path = Some(path);
        self.save();
    }
}

/// How to open a disc from the library.
pub enum DiscSource {
    /// A bare .cue file (with .bin/.cdg alongside it).
    Cue(PathBuf),
    /// A ZIP archive (including TorrentZip) containing the disc image files.
    Zip(PathBuf),
    /// A 7z archive (including Torrent7z) containing the disc image files.
    SevenZ(PathBuf),
}

/// A disc found in the library.
pub struct DiscEntry {
    pub title:  String,
    pub source: DiscSource,
}

/// Scan the library directory for discs and return them sorted by title.
///
/// Finds two kinds:
/// - Subdirectories containing a .cue file (existing behaviour).
/// - .zip files (at the library root or one level deep) that contain a .cue.
pub fn scan_library(library: &Path) -> Vec<DiscEntry> {
    let mut discs = Vec::new();
    let Ok(entries) = std::fs::read_dir(library) else { return discs };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            // ── Subdirectory: look for a .cue inside it ───────────────────
            let Ok(children) = std::fs::read_dir(&path) else { continue };
            let mut children: Vec<_> = children.flatten().map(|e| e.path()).collect();
            children.sort();

            // Plain .cue files first.
            let cue = children.iter().find(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("cue")
            });
            if let Some(cue_path) = cue {
                let title = path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                discs.push(DiscEntry { title, source: DiscSource::Cue(cue_path.clone()) });
                continue; // prefer .cue over any ZIPs in the same folder
            }

            // No bare .cue — check for ZIPs / 7z files inside the subdirectory.
            for child in &children {
                match child.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
                    Some("zip") if zip_contains_cue(child) => {
                        let title = child.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                        discs.push(DiscEntry { title, source: DiscSource::Zip(child.clone()) });
                        break;
                    }
                    Some("7z") if sevenz_contains_cue(child) => {
                        let title = child.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                        discs.push(DiscEntry { title, source: DiscSource::SevenZ(child.clone()) });
                        break;
                    }
                    _ => {}
                }
            }
        } else {
            match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
                // ── Top-level ZIP ─────────────────────────────────────────
                Some("zip") if zip_contains_cue(&path) => {
                    let title = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                    discs.push(DiscEntry { title, source: DiscSource::Zip(path.clone()) });
                }
                // ── Top-level 7z ──────────────────────────────────────────
                Some("7z") if sevenz_contains_cue(&path) => {
                    let title = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                    discs.push(DiscEntry { title, source: DiscSource::SevenZ(path.clone()) });
                }
                _ => {}
            }
        }
    }

    discs.sort_by(|a, b| a.title.cmp(&b.title));
    discs
}

/// Returns true if the ZIP archive contains at least one .cue entry.
fn zip_contains_cue(zip_path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(zip_path) else { return false };
    let Ok(archive) = zip::ZipArchive::new(file) else { return false };
    (0..archive.len()).any(|i| {
        archive.name_for_index(i)
            .map(|n| n.to_ascii_lowercase().ends_with(".cue"))
            .unwrap_or(false)
    })
}

/// Returns true if the 7z archive contains at least one .cue entry.
fn sevenz_contains_cue(path: &Path) -> bool {
    let Ok(reader) = sevenz_rust2::ArchiveReader::open(path, sevenz_rust2::Password::empty())
    else { return false };
    reader.archive().files.iter().any(|e| e.name().to_ascii_lowercase().ends_with(".cue"))
}
