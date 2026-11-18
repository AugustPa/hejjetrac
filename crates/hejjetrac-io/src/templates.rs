//! Legacy equal-area sphere-map image interchange. Pixel values retain confidence.
use crate::{Error, Result};
use hejjetrac_core::SphereTemplate;
use image::{ImageFormat, ImageReader, Limits};
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::{BufReader, BufWriter, ErrorKind},
    path::{Path, PathBuf},
};
pub fn load_sphere_map(path: &Path) -> Result<SphereTemplate> {
    let file = std::fs::File::open(path)?;
    let mut reader = ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .map_err(|e| {
            Error::Video(format!(
                "cannot identify sphere template {}: {e}",
                path.display()
            ))
        })?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(900);
    limits.max_image_height = Some(450);
    limits.max_alloc = Some(16_000_000);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|e| {
            Error::Video(format!(
                "cannot decode sphere template {}: {e}",
                path.display()
            ))
        })?
        .to_luma8();
    Ok(SphereTemplate::new(
        image.width() as usize,
        image.height() as usize,
        image.into_raw(),
    )?)
}
/// Creates a new image, refusing to overwrite an existing experiment artifact.
/// PNG is recommended: JPEG changes confidence values through lossy compression.
pub fn save_sphere_map(path: &Path, template: &SphereTemplate) -> Result<()> {
    let image = sphere_image(template)?;
    let format = ImageFormat::from_path(path)
        .map_err(|e| Error::Video(format!("sphere-map extension: {e}")))?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Bmp | ImageFormat::Pnm
    ) {
        return Err(Error::Video(
            "save sphere maps as lossless .png, .bmp, or .pgm; JPEG would alter confidence values"
                .into(),
        ));
    }
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    write_image(file, &image, format)
}

fn sphere_image(template: &SphereTemplate) -> Result<image::GrayImage> {
    template.validate()?;
    image::GrayImage::from_raw(
        template.width as u32,
        template.height as u32,
        template.pixels.clone(),
    )
    .ok_or_else(|| Error::Video("invalid sphere template storage".into()))
}

fn write_image(file: std::fs::File, image: &image::GrayImage, format: ImageFormat) -> Result<()> {
    let mut out = BufWriter::new(file);
    image
        .write_to(&mut out, format)
        .map_err(|e| Error::Video(format!("cannot encode sphere template: {e}")))?;
    std::io::Write::flush(&mut out)?;
    Ok(())
}

/// Convert a rig's display label into a single portable filename component.
pub fn atlas_filename_stem(name: &str) -> String {
    let mut stem = String::new();
    for character in name.trim().chars().take(64) {
        let character = if character.is_control() || "<>:\"/\\|?*".contains(character) {
            '_'
        } else {
            character
        };
        if stem.len() + character.len_utf8() > 200 {
            break;
        }
        stem.push(character);
    }
    let trimmed = stem.trim_matches([' ', '.']);
    let mut stem = if trimmed.is_empty() {
        "atlas".to_owned()
    } else {
        trimmed.to_owned()
    };
    let base = stem
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || base
            .strip_prefix("COM")
            .or_else(|| base.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
    {
        stem.insert(0, '_');
    }
    stem
}

fn directory_unavailable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::PermissionDenied
            | ErrorKind::ReadOnlyFilesystem
            | ErrorKind::NotFound
            | ErrorKind::NotADirectory
            | ErrorKind::AlreadyExists
    )
}

fn directory_error(directory: &Path, error: std::io::Error) -> Error {
    std::io::Error::new(
        error.kind(),
        format!(
            "Cannot save an atlas in {}: {error}. Choose a writable atlas folder.",
            directory.display()
        ),
    )
    .into()
}

/// Save a new named PNG, allocating `name.png`, `name-2.png`, and so on.
/// Existing names are compared without case; create_new handles competing writers.
/// A second folder is used only when directory access or initial file creation
/// is unavailable. Invalid maps and encoding/write failures are returned directly.
pub fn save_sphere_map_unique(
    directories: &[PathBuf],
    name: &str,
    template: &SphereTemplate,
) -> Result<PathBuf> {
    if !(1..=2).contains(&directories.len()) {
        return Err(Error::Config(
            "provide one atlas folder and at most one fallback folder".into(),
        ));
    }
    // Validate the numerical image before creating folders or reserving a name.
    let image = sphere_image(template)?;
    let stem = atlas_filename_stem(name);
    let mut unavailable = None;
    'directory: for directory in directories {
        let entries =
            match std::fs::create_dir_all(directory).and_then(|()| std::fs::read_dir(directory)) {
                Ok(entries) => entries,
                Err(error) if directory_unavailable(&error) => {
                    unavailable = Some(directory_error(directory, error));
                    continue;
                }
                Err(error) => return Err(directory_error(directory, error)),
            };
        let mut occupied = BTreeSet::new();
        for (count, entry) in entries.enumerate() {
            if count >= 100_000 {
                return Err(Error::Config(format!(
                    "Atlas folder {} has too many entries; choose another folder.",
                    directory.display()
                )));
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if directory_unavailable(&error) => {
                    unavailable = Some(directory_error(directory, error));
                    continue 'directory;
                }
                Err(error) => return Err(directory_error(directory, error)),
            };
            occupied.insert(entry.file_name().to_string_lossy().to_lowercase());
        }
        for number in 1..=10_000 {
            let filename = if number == 1 {
                format!("{stem}.png")
            } else {
                format!("{stem}-{number}.png")
            };
            if occupied.contains(&filename.to_lowercase()) {
                continue;
            }
            let path = directory.join(&filename);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    // Do not retry another folder after creating a file: a
                    // partial encode/write must remain an explicit failure.
                    write_image(file, &image, ImageFormat::Png)?;
                    return Ok(path);
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    occupied.insert(filename.to_lowercase());
                }
                Err(error) if directory_unavailable(&error) => {
                    unavailable = Some(directory_error(directory, error));
                    continue 'directory;
                }
                Err(error) => return Err(directory_error(directory, error)),
            }
        }
        return Err(Error::Config(format!(
            "Atlas names for {stem} in {} are exhausted; choose another folder.",
            directory.display()
        )));
    }
    Err(unavailable
        .unwrap_or_else(|| Error::Config("No writable atlas folder is available".into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    };

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> std::io::Result<Self> {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "hejjetrac-map-defaults-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path)?;
            Ok(Self(path))
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn material() -> Result<SphereTemplate> {
        Ok(SphereTemplate::new(
            30,
            15,
            (0..450).map(|index| (index % 256) as u8).collect(),
        )?)
    }

    #[test]
    fn named_png_exports_preserve_case_collisions_and_confidence_pixels() -> TestResult {
        let temp = Temp::new()?;
        let directory = temp.0.join("atlases");
        std::fs::create_dir(&directory)?;
        let existing = directory.join("vr2.png");
        std::fs::write(&existing, b"retained existing atlas")?;
        let map = material()?;
        let first = save_sphere_map_unique(std::slice::from_ref(&directory), "VR2", &map)?;
        let second = save_sphere_map_unique(std::slice::from_ref(&directory), "VR2", &map)?;
        assert_eq!(first, directory.join("VR2-2.png"));
        assert_eq!(second, directory.join("VR2-3.png"));
        assert_eq!(std::fs::read(existing)?, b"retained existing atlas");
        assert_eq!(load_sphere_map(&first)?, map);
        assert_eq!(load_sphere_map(&second)?, map);
        Ok(())
    }

    #[test]
    fn concurrent_named_exports_reserve_distinct_complete_pngs() -> TestResult {
        let temp = Temp::new()?;
        let directory = temp.0.join("atlases");
        std::fs::create_dir(&directory)?;
        let map = Arc::new(material()?);
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let directory = directory.clone();
            let map = Arc::clone(&map);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                save_sphere_map_unique(&[directory], "VR2", &map)
            }));
        }
        let mut paths = BTreeSet::new();
        for handle in handles {
            let path = handle.join().map_err(|_| "atlas export thread failed")??;
            assert_eq!(load_sphere_map(&path)?, *map);
            paths.insert(path);
        }
        assert_eq!(paths.len(), 4);
        for name in ["VR2.png", "VR2-2.png", "VR2-3.png", "VR2-4.png"] {
            assert!(paths.contains(&directory.join(name)));
        }
        Ok(())
    }

    #[test]
    fn unavailable_primary_uses_fallback_and_invalid_maps_create_nothing() -> TestResult {
        let temp = Temp::new()?;
        let blocked = temp.0.join("not-a-directory");
        std::fs::write(&blocked, b"retained file")?;
        let fallback = temp.0.join("fallback/atlases");
        let map = material()?;
        let path = save_sphere_map_unique(&[blocked.clone(), fallback.clone()], "VR4", &map)?;
        assert_eq!(path, fallback.join("VR4.png"));
        assert_eq!(load_sphere_map(&path)?, map);
        assert_eq!(std::fs::read(blocked)?, b"retained file");
        let untouched = temp.0.join("must-not-be-created");
        let invalid = SphereTemplate {
            width: 30,
            height: 15,
            pixels: vec![128; 450],
        };
        assert!(save_sphere_map_unique(std::slice::from_ref(&untouched), "VR4", &invalid).is_err());
        assert!(!untouched.exists());
        Ok(())
    }

    #[test]
    fn display_labels_are_portable_single_filename_components() {
        assert_eq!(atlas_filename_stem("VR7"), "VR7");
        assert_eq!(atlas_filename_stem("Rig A"), "Rig A");
        assert_eq!(atlas_filename_stem("../Rig: A/\\?*\0"), "_Rig_ A_____");
        assert_eq!(atlas_filename_stem(" ... "), "atlas");
        assert_eq!(atlas_filename_stem("CON"), "_CON");
        assert_eq!(atlas_filename_stem("com1.test"), "_com1.test");
        let stem = atlas_filename_stem(&"界".repeat(100));
        assert!(stem.len() <= 200);
        assert_eq!(Path::new(&stem).components().count(), 1);
    }
}
