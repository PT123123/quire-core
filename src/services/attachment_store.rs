// The attachments folder (SPEC §三十七 批次 A, §十八's reserved table).
//
// One file per attachment, next to the database; the `attachments` row is the
// only thing SQLite keeps. Three rules drive the shape of this module:
//
// - A picture must never reach the UI at its original size (§二十二). Every
//   import that exceeds `MAX_EDGE` on either side writes a second, downscaled
//   PNG, and `Attachment::thumb` names it. The original stays untouched on
//   disk so the user's bytes are never degraded by viewing them.
// - A file that is not a picture is never decoded at all. `import_any_file`
//   streams the bytes with `fs::copy`, because a multi-gigabyte attachment
//   must not cost a multi-gigabyte working set — the same §二十二 rule that
//   shapes the picture path, arriving from the other direction.
// - Nothing here is allowed to block the editor. Import is called from a user
//   action (pick / paste), never from a projection or a paint.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageFormat};

use crate::core::types::{Attachment, AttachmentId};
// Only `create_file_fixture` below uses it, but that helper is not
// `#[cfg(test)]`: the headless screenshot tool builds against the release-ish
// lib, so its throwaway folder needs the same self-deleting guard the tests
// get.
use crate::testing::ScratchDir;

/// Longest edge of the raster the editor loads. The editor column is ~780 px
/// at the default window, so this is a ≈1.6× margin for a full-width picture
/// on a HiDPI screen — and 1280×1280 RGBA is 6.5 MB, which a page of pictures
/// can afford. Bigger than this and the downsample stops paying for itself.
pub const MAX_EDGE: u32 = 1280;

/// Anything the store could not do: an unreadable file, a format we cannot
/// decode, a disk that refused the write. The caller shows one line to the
/// user, so the strings are written to be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentError(String);

impl std::fmt::Display for AttachmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn err(msg: impl Into<String>) -> AttachmentError {
    AttachmentError(msg.into())
}

pub struct AttachmentStore {
    dir: PathBuf,
}

impl AttachmentStore {
    /// `<library>/attachments` beside the database. With no database (the
    /// headless shot tool, a memory-only test) the folder is the system temp
    /// directory, which keeps the store usable without inventing a path.
    pub fn for_db(db: Option<&Path>) -> Self {
        let dir = match db.and_then(|p| p.parent()) {
            Some(parent) => parent.join("attachments"),
            None => std::env::temp_dir().join("quire-attachments"),
        };
        AttachmentStore { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Read a picked file and store it. The name the user sees is the file's
    /// own stem, taken before decoding so an undecodable file still reports
    /// what the user asked for.
    pub fn import_file(
        &self,
        id: AttachmentId,
        path: &Path,
    ) -> Result<Attachment, AttachmentError> {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".into());
        let bytes = fs::read(path)
            .map_err(|e| err(format!("could not read {name}: {e}")))?;
        self.import_bytes(id, &name, &bytes)
    }

    /// Store already-held bytes (a clipboard bitmap, a generated fixture).
    pub fn import_bytes(
        &self,
        id: AttachmentId,
        name: &str,
        bytes: &[u8],
    ) -> Result<Attachment, AttachmentError> {
        // Decode by content, not by extension: a file named .jpg that holds a
        // PNG is stored as the PNG it is.
        let img = image::load_from_memory(bytes)
            .map_err(|e| err(format!("{name}: not an image Quire can read ({e})")))?;
        let format = sniff_format(bytes);
        let (width, height) = (img.width(), img.height());
        if width == 0 || height == 0 {
            return Err(err(format!("{name}: empty picture")));
        }
        fs::create_dir_all(&self.dir)
            .map_err(|e| err(format!("could not create the attachments folder: {e}")))?;

        let file = format!("{}.{ext}", id.as_u64(), ext = ext_of(format));
        fs::write(self.dir.join(&file), bytes)
            .map_err(|e| err(format!("could not store {name}: {e}")))?;

        // The raster the editor loads: the original when it already fits, a
        // downscaled PNG when it does not.
        let thumb = if width.max(height) <= MAX_EDGE {
            String::new()
        } else {
            let scale = MAX_EDGE as f32 / width.max(height) as f32;
            let (tw, th) = (
                ((width as f32 * scale).round().max(1.0)) as u32,
                ((height as f32 * scale).round().max(1.0)) as u32,
            );
            let small = img.resize_exact(tw, th, image::imageops::FilterType::Lanczos3);
            let mut png = Vec::new();
            small
                .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
                .map_err(|e| err(format!("could not encode {name}: {e}")))?;
            let name = format!("{}.cache.png", id.as_u64());
            fs::write(self.dir.join(&name), png)
                .map_err(|e| err(format!("could not store the preview of {name}: {e}")))?;
            name
        };

        Ok(Attachment {
            id,
            name: name.to_string(),
            file,
            thumb,
            mime: mime_of(format).to_string(),
            bytes: bytes.len() as i64,
            width,
            height,
        })
    }

    /// The file the UI should load for this attachment: the downscaled copy
    /// when there is one, the original otherwise.
    pub fn display_path(&self, att: &Attachment) -> PathBuf {
        self.dir.join(if att.thumb.is_empty() {
            &att.file
        } else {
            &att.thumb
        })
    }

    /// The bytes as they were imported — what "open" and "save as" hand to the
    /// system. Never the display copy: a user opening a picture gets their
    /// original, not the 1280 px raster the editor painted.
    pub fn stored_path(&self, att: &Attachment) -> PathBuf {
        self.dir.join(&att.file)
    }

    /// Store an arbitrary file (SPEC §三十七 批次 A's `file` kind). Nothing is
    /// decoded and nothing is held in memory: the bytes are streamed straight
    /// into the attachments folder, because a 2 GB attachment must not cost
    /// 2 GB of working set (§二十二). Unlike a picture, the label keeps the
    /// extension: for a picture the content is the truth, for a `.zip` the
    /// name is, and a row that reads "quarterly-report" tells you nothing
    /// about what pressing Open will bring up.
    pub fn import_any_file(
        &self,
        id: AttachmentId,
        path: &Path,
    ) -> Result<Attachment, AttachmentError> {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let meta = fs::metadata(path).map_err(|e| err(format!("could not read {name}: {e}")))?;
        if !meta.is_file() {
            return Err(err(format!("{name}: not a file")));
        }
        fs::create_dir_all(&self.dir)
            .map_err(|e| err(format!("could not create the attachments folder: {e}")))?;

        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .filter(|e| !e.is_empty() && e.len() <= 8)
            .unwrap_or_else(|| "bin".into());
        let file = format!("{}.{ext}", id.as_u64());
        let to = self.dir.join(&file);
        let copied = fs::copy(path, &to).map_err(|e| err(format!("could not store {name}: {e}")))?;
        if copied != meta.len() {
            let _ = fs::remove_file(&to);
            return Err(err(format!("{name}: the copy came up short")));
        }

        Ok(Attachment {
            id,
            name: name.to_string(),
            file,
            thumb: String::new(),
            mime: mime_of_ext(&ext).to_string(),
            bytes: meta.len() as i64,
            width: 0,
            height: 0,
        })
    }

    /// The name to offer a "save as" dialog. A picture stores its stem in
    /// `name` and the extension in `file` (`<id>.<ext>`), so the two have to be
    /// rejoined; a name that already carries the extension — every file import
    /// does — is left alone. The compare ignores case because `file` got its
    /// extension lowercased and `name` kept the user's.
    pub fn save_name(&self, att: &Attachment) -> String {
        let Some((_, ext)) = att.file.rsplit_once('.') else {
            return att.name.clone();
        };
        if att.name.rsplit_once('.').is_some_and(|(stem, e)| e.eq_ignore_ascii_case(ext) && !stem.is_empty()) {
            return att.name.clone();
        }
        format!("{}.{ext}", att.name)
    }

    /// Copy a stored attachment out to a path the user chose ("save as").
    pub fn export_to(&self, att: &Attachment, target: &Path) -> Result<(), AttachmentError> {
        let from = self.stored_path(att);
        fs::copy(&from, target).map_err(|e| {
            err(format!(
                "could not write {} to that location: {e}",
                self.save_name(att)
            ))
        })?;
        Ok(())
    }

    /// A deterministic non-picture file for the headless scenes: written to a
    /// temporary path and run through the same import a picked file takes, so
    /// the row under test is built by the code a user exercises. `name` is the
    /// whole file name, extension included, because that is what decides the
    /// label and the size the scene is there to check.
    pub fn create_file_fixture(
        &self,
        id: AttachmentId,
        name: &str,
        payload: &[u8],
    ) -> Option<Attachment> {
        // Nothing of the source path reaches the attachment row except the file
        // name, so the unique part of the fixture lives in the folder instead:
        // a pid in the name would change the label, and the label is painted.
        let dir = ScratchDir::new("fixture");
        let source = dir.join(name);
        fs::write(&source, payload).ok()?;
        self.import_any_file(id, &source).ok()
    }

    /// A deterministic picture for the headless scenes: a two-colour diagonal
    /// gradient, encoded once and run through the same import path a picked
    /// file takes. Returns `None` if the encode or the store failed.
    pub fn create_fixture(&self, id: AttachmentId, width: u32, height: u32) -> Option<Attachment> {
        let mut buf = image::RgbImage::new(width, height);
        {
            let denom = (width + height).max(2) as f32;
            let span = (width + height).max(1);
            for (x, y, pixel) in buf.enumerate_pixels_mut() {
                let t = ((x + y) % span) as f32 / denom;
                *pixel = image::Rgb([
                    (t * 200.0) as u8,
                    90,
                    (255.0 - t * 160.0) as u8,
                ]);
            }
        }
        let mut png = Vec::new();
        DynamicImage::ImageRgb8(buf)
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .ok()?;
        self.import_bytes(id, "sample-gradient", &png).ok()
    }
}

/// Name the format from the bytes themselves (the decoder figured this out
/// while loading; `DynamicImage` just does not hand the answer back on the
/// image version we pin). Guessing from the file name is the bug this avoids:
/// a screenshot saved as `.jpg` but encoded PNG would be stored as a lie.
///
/// The list is what `Cargo.toml` asks `image` to decode; anything else that
/// still got through lands on the fallback. GIF is in it as a still: the
/// decoder hands back the first frame and nothing in the editor ticks.
fn sniff_format(bytes: &[u8]) -> Option<ImageFormat> {
    image::guess_format(bytes).ok()
}

fn ext_of(format: Option<ImageFormat>) -> &'static str {
    match format {
        Some(ImageFormat::Png) => "png",
        Some(ImageFormat::Jpeg) => "jpg",
        Some(ImageFormat::Bmp) => "bmp",
        Some(ImageFormat::Gif) => "gif",
        _ => "img",
    }
}

fn mime_of(format: Option<ImageFormat>) -> &'static str {
    match format {
        Some(ImageFormat::Png) => "image/png",
        Some(ImageFormat::Jpeg) => "image/jpeg",
        Some(ImageFormat::Bmp) => "image/bmp",
        Some(ImageFormat::Gif) => "image/gif",
        _ => "application/octet-stream",
    }
}

/// The type a stored file announces itself as. Deliberately short: the app
/// has no behaviour keyed on MIME yet — a PDF is stored and shown exactly
/// like a `.zip` until §三十七 批次 A's first-page thumbnail lands, which is
/// the only thing that will need to ask. Everything unnamed is honestly
/// octet-stream.
fn mime_of_ext(ext: &str) -> &'static str {
    match ext {
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Human-readable size for the file row ("1.4 MiB"). Binary units, because the
/// number is a byte count the user may cross-check against Explorer's.
pub fn format_size(bytes: i64) -> String {
    let b = bytes.max(0) as f64;
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if b < 1024.0 {
        return format!("{b:.0} B");
    }
    let mut v = b;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    let text = if v >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    };
    format!("{} {}", text.trim_end_matches(".0"), UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store in its own throwaway folder, beside a fake database name so
    /// `for_db` takes the real `<library>/attachments` branch.
    ///
    /// The folder is a guard rather than a path the test drops: each of these
    /// used to end with `remove_dir_all(&dir).unwrap()`, which a failing
    /// assertion skipped, so every red run left its pictures in `%TEMP%`.
    fn store() -> (ScratchDir, AttachmentStore) {
        let dir = ScratchDir::new("att");
        let store = AttachmentStore::for_db(Some(&dir.join("library.db")));
        assert_eq!(store.dir(), dir.join("attachments"));
        (dir, store)
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let buf = image::RgbImage::from_pixel(w, h, image::Rgb([200, 30, 40]));
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(buf)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn a_picture_that_fits_is_stored_as_is() {
        let (_dir, store) = store();
        let bytes = png(640, 480);
        let att = store.import_bytes(AttachmentId(3), "screenshot", &bytes).unwrap();
        assert_eq!(att.file, "3.png");
        assert_eq!(att.thumb, "", "no cache copy for a picture that already fits");
        assert_eq!((att.width, att.height), (640, 480));
        assert_eq!(att.mime, "image/png");
        assert_eq!(att.bytes as usize, bytes.len());
        assert_eq!(att.name, "screenshot", "the display name comes from the caller");

        let display = store.display_path(&att);
        assert_eq!(display, store.dir().join("3.png"));
        assert_eq!(image::image_dimensions(&display).unwrap(), (640, 480));
    }

    #[test]
    fn an_oversized_picture_gets_a_cache_copy_and_keeps_its_original() {
        let (_dir, store) = store();
        // one pixel over the limit: the branch boundary is the interesting part
        let bytes = png(MAX_EDGE + 1, 7);
        let att = store.import_bytes(AttachmentId(9), "big", &bytes).unwrap();
        assert_eq!(att.thumb, "9.cache.png");

        // the editor must never be handed the 1281 px raster
        let display = store.display_path(&att);
        assert_eq!(display, store.dir().join("9.cache.png"));
        assert_eq!(image::image_dimensions(&display).unwrap(), (MAX_EDGE, 7));
        // ...but viewing must not degrade the user's bytes
        assert_eq!(
            image::image_dimensions(store.dir().join(&att.file)).unwrap(),
            (MAX_EDGE + 1, 7),
            "the stored original is the file that was imported"
        );
        assert_eq!(
            fs::read(store.dir().join(&att.file)).unwrap().len(),
            bytes.len()
        );
    }

    #[test]
    fn the_stored_extension_follows_the_bytes_not_the_name() {
        let (_dir, store) = store();
        // `import_file` takes the stem, so a screenshot saved as .jpg but
        // encoded PNG has to be stored as the PNG it really is.
        let bytes = png(8, 8);
        let att = store.import_bytes(AttachmentId(1), "lying-name.jpg", &bytes).unwrap();
        assert_eq!(att.file, "1.png");
        assert_eq!(att.mime, "image/png");
        assert_eq!(att.name, "lying-name.jpg", "the name is the user's, the file is ours");
    }

    #[test]
    fn a_gif_is_stored_as_a_still_gif() {
        let (_dir, store) = store();
        // The GIF arms in `ext_of` / `mime_of` would compile either way; this is
        // what proves the decoder is really there. Written with the same crate
        // that reads it back, so a build without the format fails the encode
        // rather than quietly passing an assertion about a branch that cannot
        // be reached.
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 3, image::Rgb([7, 8, 9])))
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Gif)
            .expect("gif encoding is compiled in");
        let att = store.import_bytes(AttachmentId(30), "spinner", &bytes).unwrap();
        assert_eq!(att.file, "30.gif");
        assert_eq!(att.mime, "image/gif");
        assert_eq!(
            (att.width, att.height),
            (4, 3),
            "the first frame, and no animation clock anywhere in the app"
        );
    }

    #[test]
    fn bytes_that_are_not_a_picture_are_rejected_without_touching_disk() {
        let (_dir, store) = store();
        let e = store.import_bytes(AttachmentId(2), "notes", b"just text").unwrap_err();
        assert!(e.to_string().contains("notes"), "the message names the file: {e}");
        assert!(!store.dir().join("2.img").exists());
    }

    #[test]
    fn a_fixture_picture_is_reproducible_and_importable() {
        let (_dir, store) = store();
        let att = store.create_fixture(AttachmentId(6), 120, 90).unwrap();
        assert_eq!((att.width, att.height), (120, 90));
        assert_eq!(image::image_dimensions(store.display_path(&att)).unwrap(), (120, 90));
        // the same id re-imports over itself rather than failing
        let again = store.create_fixture(AttachmentId(6), 120, 90).unwrap();
        assert_eq!(again.file, att.file);
    }

    #[test]
    fn an_arbitrary_file_is_copied_verbatim_and_never_decoded() {
        let (dir, store) = store();
        // bytes no image decoder would accept, under a name with a dot in it
        let source = dir.join("quarterly.report.pdf");
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&source, &payload).unwrap();

        let att = store.import_any_file(AttachmentId(12), &source).unwrap();
        assert_eq!(
            att.name, "quarterly.report.pdf",
            "a file keeps its extension: the name is the only clue to what it is"
        );
        assert_eq!(att.file, "12.pdf", "the extension is the user's, not sniffed");
        assert_eq!(att.mime, "application/pdf");
        assert_eq!(att.bytes, payload.len() as i64);
        assert_eq!((att.width, att.height), (0, 0), "nothing was decoded");
        assert_eq!(att.thumb, "", "there is no display copy of a PDF");
        assert_eq!(fs::read(store.stored_path(&att)).unwrap(), payload);

        // and it round-trips back out under a name worth saving
        let out = dir.join("elsewhere.pdf");
        store.export_to(&att, &out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), payload);
    }

    #[test]
    fn a_file_import_leaves_nothing_behind_when_the_copy_fails() {
        let (dir, store) = store();
        // a directory is not a file: the check must fire before the copy, so
        // there is no half-written attachment to find later
        let e = store.import_any_file(AttachmentId(13), &dir).unwrap_err();
        assert!(e.to_string().contains("not a file"), "{e}");
        assert!(!store.dir().join("13.bin").exists());

        // an extensionless file is still storable, under an honest fallback
        let source = dir.join("LICENSE");
        fs::write(&source, b"MIT").unwrap();
        let att = store.import_any_file(AttachmentId(14), &source).unwrap();
        assert_eq!(att.file, "14.bin");
        assert_eq!(att.mime, "application/octet-stream");
        assert_eq!(att.name, "LICENSE");
    }

    #[test]
    fn the_save_name_rejoins_what_the_picture_import_split() {
        let (dir, store) = store();
        // A picture is stored under its stem — the extension lives only in the
        // `file` column — so the save dialog has to put the pair back together.
        let png = {
            let mut buf = Vec::new();
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 3, image::Rgb([9, 9, 9])))
                .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
                .unwrap();
            buf
        };
        let pic = store.import_bytes(AttachmentId(21), "sunset", &png).unwrap();
        assert_eq!(pic.name, "sunset", "the row holds the stem");
        assert_eq!(store.save_name(&pic), "sunset.png", "the dialog offers the pair");

        // A file import already carries its extension, so nothing is appended —
        // whatever the case the user typed it in.
        let source = dir.join("Report.PDF");
        fs::write(&source, b"%PDF-1.4").unwrap();
        let file = store.import_any_file(AttachmentId(22), &source).unwrap();
        assert_eq!(file.file, "22.pdf", "the stored name is lowercased");
        assert_eq!(store.save_name(&file), "Report.PDF", "the label is not");

        // and a name with no extension at all keeps the name it was given
        let bare = Attachment {
            file: "23".into(),
            ..file.clone()
        };
        assert_eq!(store.save_name(&bare), "Report.PDF");
    }

    #[test]
    fn sizes_read_the_way_a_user_checks_them() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1024), "1 KiB");
        assert_eq!(format_size(1024 + 512), "1.5 KiB");
        assert_eq!(format_size(1024 * 1024 * 3 / 2), "1.5 MiB");
        // above 100 the decimal buys nothing but width
        assert_eq!(format_size(1024 * 1024 * 240), "240 MiB");
        assert_eq!(format_size(1024i64 * 1024 * 1024 * 4), "4 GiB");
        // a row whose byte count went missing reads as empty, not as negative
        assert_eq!(format_size(-5), "0 B");
    }
}
