//! Rendering and remembering the small previews the gallery shows.
//!
//! A frame is a Raspberry Pi that is already decoding and resizing a full-screen photo every few
//! seconds, so a gallery must not ask it to decode a 12 megapixel JPEG per tile. Two things keep
//! that from happening: the preview a camera or phone already embedded in the file is used when
//! there is one, and whatever was rendered is kept until the memory budget runs out.

use std::{
    collections::VecDeque,
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
    sync::{Mutex, MutexGuard, PoisonError},
};

use anyhow::Result;
use bytes::Bytes;
use image::codecs::jpeg::JpegEncoder;

use crate::img::{self, DynamicImage, Framed};

/// Longest edge of a rendered preview. Enough for a grid tile on a high resolution phone screen,
/// while a photo scaled to this is a few kilobytes rather than a few megabytes.
const THUMBNAIL_SIZE: u32 = 320;

/// An embedded preview smaller than this is not worth showing, so the photo itself is decoded
/// instead. Cameras embed 160x120, phones usually more.
const MIN_EMBEDDED_SIZE: u32 = 160;

/// Previews are viewed on a phone over a home network, where the difference between this and a
/// higher setting is invisible and the difference in bytes is not
const JPEG_QUALITY: u8 = 75;

/// Rendered previews, kept until the budget is used up
pub struct Thumbnails {
    cache: Mutex<Cache>,
    /// [None] when nothing is kept across restarts, which is every build that has nowhere to put
    /// it. The memory cache sits in front of this either way.
    disk: Option<DiskCache>,
}

impl Thumbnails {
    pub fn new(budget: usize) -> Self {
        Self {
            cache: Mutex::new(Cache::new(budget)),
            disk: None,
        }
    }

    /// Keeps rendered previews in `directory`, so that a restart does not throw away work that
    /// cost a download each. On this album 43 photos in 99 carry no embedded preview and have to
    /// be fetched whole, about a second apiece; paying that once rather than once per restart is
    /// the whole point.
    pub fn with_disk_cache(mut self, directory: PathBuf, budget: u64) -> Self {
        let disk = DiskCache::new(directory, budget);
        disk.prune();
        self.disk = Some(disk);
        self
    }

    /// The preview of a photo, rendering it if it has not been asked for recently.
    ///
    /// `load` fetches the photo itself and is only called on a miss, which is the expensive part:
    /// it is a download over FTP.
    pub fn get(
        &self,
        key: &str,
        load_head: impl FnOnce() -> Result<Bytes>,
        load_all: impl FnOnce() -> Result<Bytes>,
    ) -> Result<Bytes> {
        if let Some(cached) = self.lock().take_recent(key) {
            return Ok(cached);
        }
        if let Some(cached) = self.disk.as_ref().and_then(|disk| disk.read(key)) {
            self.lock().insert(key.to_string(), cached.clone());
            return Ok(cached);
        }
        /* Nearly every photo carries a preview in its first few kilobytes, and rendering from it
         * takes single-digit milliseconds. Downloading the whole file instead cost about a second
         * each on a Raspberry Pi reading a NAS, which for a gallery is the entire wait. The whole
         * file is only fetched for the photos that have no usable preview. */
        let from_head = match load_head() {
            Ok(head) => render_embedded(&head),
            /* Reading only part of a file asks more of a server than reading all of it, so when
             * that goes wrong the photo is still perfectly fetchable the ordinary way. Giving up
             * here would turn a slow tile into a broken one. */
            Err(error) => {
                log::debug!("Could not read the start of {key}: {error:#}");
                None
            }
        };
        let thumbnail = match from_head {
            Some(thumbnail) => thumbnail,
            None => {
                log::debug!("No usable preview in the start of {key}, downloading all of it");
                render(&load_all()?)?
            }
        };
        let thumbnail = Bytes::from(thumbnail);
        if let Some(disk) = &self.disk {
            disk.write(key, &thumbnail);
        }
        self.lock().insert(key.to_string(), thumbnail.clone());
        Ok(thumbnail)
    }

    /// Drops the preview of a photo that is gone.
    ///
    /// An upload never overwrites, it picks a free name, so the bytes behind a path do not change
    /// on their own. A path can still be *reused*: delete a directory, create one with the same
    /// name, upload a photo whose name was seen before, and the old preview would be served for
    /// the new photo. Deletion is therefore the one thing that has to invalidate, and it has to
    /// invalidate whole subtrees, not just the photo named.
    pub fn forget(&self, key: &str) {
        self.lock().remove(key);
        if let Some(disk) = &self.disk {
            disk.remove(key);
        }
    }

    /// Drops the previews of everything that was under a directory that is gone.
    pub fn forget_under(&self, directory: &str) {
        let prefix = format!("{directory}/");
        self.lock().remove_under(&prefix);
        if let Some(disk) = &self.disk {
            disk.remove_under(&prefix);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Characters that cannot be part of a file name, or that would make the escaping ambiguous.
///
/// Everything else is left alone, so a thumbnail's file is named after the photo it belongs to and
/// the cache can be read by a human wondering what is in it.
const ESCAPED_IN_FILE_NAMES: &percent_encoding::AsciiSet =
    &percent_encoding::CONTROLS.add(b'%').add(b'/');

/// A file name longer than this is refused by some filesystems, and a photo buried that deep is
/// not worth special handling: it is cached in memory like everything else, just not on disk.
const LONGEST_FILE_NAME: usize = 200;

/// How many writes go by before the directory is measured again. Pruning reads the whole
/// directory, which is cheap but not free, and the budget is a limit rather than a promise.
const WRITES_BETWEEN_PRUNES: usize = 128;

/// Rendered previews kept as files, so a restart does not throw the work away.
///
/// Every operation here is best-effort: a cache that cannot be read or written is a slower frame,
/// never a broken one, so failures are logged and otherwise ignored.
struct DiskCache {
    directory: PathBuf,
    budget: u64,
    writes: AtomicUsize,
}

impl DiskCache {
    fn new(directory: PathBuf, budget: u64) -> Self {
        Self {
            directory,
            budget,
            writes: AtomicUsize::new(0),
        }
    }

    /// The file a photo's thumbnail belongs in, or [None] when its path cannot be one.
    ///
    /// The photo's own path is the name, with the two characters that cannot appear in one
    /// escaped. Keeping the path recoverable is what lets a deleted directory's thumbnails be
    /// found again by their prefix, which a hash would not allow.
    fn file_for(&self, key: &str) -> Option<PathBuf> {
        let name = percent_encoding::utf8_percent_encode(key, ESCAPED_IN_FILE_NAMES).to_string();
        (name.len() <= LONGEST_FILE_NAME).then(|| self.directory.join(name))
    }

    fn key_of(file: &Path) -> Option<String> {
        let name = file.file_name()?.to_str()?;
        Some(
            percent_encoding::percent_decode_str(name)
                .decode_utf8()
                .ok()?
                .into_owned(),
        )
    }

    fn read(&self, key: &str) -> Option<Bytes> {
        let file = self.file_for(key)?;
        match fs::read(&file) {
            Ok(bytes) => Some(Bytes::from(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                log::debug!("Could not read {}: {error}", file.display());
                None
            }
        }
    }

    fn write(&self, key: &str, thumbnail: &[u8]) {
        let Some(file) = self.file_for(key) else {
            return;
        };
        if let Err(error) = fs::create_dir_all(&self.directory) {
            log::debug!("Could not make {}: {error}", self.directory.display());
            return;
        }
        /* Written beside the real name and moved onto it, so that a frame stopped mid-write never
         * leaves behind half a thumbnail that would be served as if it were whole */
        let partial = file.with_extension("partial");
        if let Err(error) =
            fs::write(&partial, thumbnail).and_then(|()| fs::rename(&partial, &file))
        {
            log::debug!("Could not keep {}: {error}", file.display());
            let _ = fs::remove_file(&partial);
            return;
        }
        if self
            .writes
            .fetch_add(1, AtomicOrdering::Relaxed)
            .is_multiple_of(WRITES_BETWEEN_PRUNES)
        {
            self.prune();
        }
    }

    fn remove(&self, key: &str) {
        if let Some(file) = self.file_for(key) {
            let _ = fs::remove_file(file);
        }
    }

    fn remove_under(&self, prefix: &str) {
        for file in self.files() {
            if Self::key_of(&file).is_some_and(|key| key.starts_with(prefix)) {
                let _ = fs::remove_file(file);
            }
        }
    }

    fn files(&self) -> Vec<PathBuf> {
        match fs::read_dir(&self.directory) {
            Ok(entries) => entries.flatten().map(|entry| entry.path()).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(error) => {
                log::debug!("Could not list {}: {error}", self.directory.display());
                vec![]
            }
        }
    }

    /// Drops the oldest thumbnails until the directory is within its budget.
    ///
    /// By the time they were written rather than the time they were last wanted: keeping a read
    /// time would mean writing to the disk every time a page is opened, which is the thing this
    /// cache exists to avoid.
    fn prune(&self) {
        let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = self
            .files()
            .into_iter()
            .filter_map(|path| {
                let data = fs::metadata(&path).ok()?;
                Some((path, data.len(), data.modified().ok()?))
            })
            .collect();
        let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
        if total <= self.budget {
            return;
        }
        files.sort_by_key(|(_, _, written)| *written);
        for (path, size, _) in files {
            if total <= self.budget {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
        log::debug!("Pruned the thumbnail cache to {total} bytes");
    }
}

/// Least recently used, bounded by the total size of what it holds rather than by a number of
/// entries: a preview of a panorama is many times the size of one of a snapshot, and it is memory
/// on a 512 MB machine that is scarce, not entries.
struct Cache {
    budget: usize,
    used: usize,
    /// Least recently used first
    entries: VecDeque<(String, Bytes)>,
}

impl Cache {
    const fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            entries: VecDeque::new(),
        }
    }

    fn take_recent(&mut self, key: &str) -> Option<Bytes> {
        let position = self.entries.iter().position(|(stored, _)| stored == key)?;
        let entry = self.entries.remove(position)?;
        let bytes = entry.1.clone();
        self.entries.push_back(entry);
        Some(bytes)
    }

    fn insert(&mut self, key: String, bytes: Bytes) {
        self.remove(&key);
        if bytes.len() > self.budget {
            /* Storing it would evict everything else and still not fit */
            return;
        }
        self.used += bytes.len();
        self.entries.push_back((key, bytes));
        while self.used > self.budget {
            let Some((_, evicted)) = self.entries.pop_front() else {
                break;
            };
            self.used -= evicted.len();
        }
    }

    fn remove(&mut self, key: &str) {
        if let Some(position) = self.entries.iter().position(|(stored, _)| stored == key)
            && let Some((_, removed)) = self.entries.remove(position)
        {
            self.used -= removed.len();
        }
    }

    fn remove_under(&mut self, prefix: &str) {
        let mut freed = 0;
        self.entries.retain(|(stored, bytes)| {
            let keep = !stored.starts_with(prefix);
            if !keep {
                freed += bytes.len();
            }
            keep
        });
        self.used -= freed;
    }
}

/// Turns the bytes of a photo into the bytes of its preview
fn render(photo: &[u8]) -> Result<Vec<u8>> {
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(photo))
        .ok();

    let image = match exif.as_ref().and_then(usable_preview) {
        /* Free next to decoding the photo itself, which on a Pi costs a second or more */
        Some(image) => image,
        None => decode(photo)?,
    };
    encode(image, exif.as_ref())
}

/// The thumbnail built from the preview the camera embedded, or [None] when the bytes given do not
/// contain a usable one.
///
/// Takes only as much of the photo as the caller happens to have, so that the start of a file is
/// enough. A truncated photo cannot be decoded, which is the point: this never falls back to
/// decoding, it reports that it could not and leaves that decision to the caller.
fn render_embedded(photo: &[u8]) -> Option<Vec<u8>> {
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(photo))
        .ok()?;
    let image = usable_preview(&exif)?;
    encode(image, Some(&exif)).ok()
}

/// The embedded preview, decoded, if there is one and it is not too small to be worth using
fn usable_preview(exif: &exif::Exif) -> Option<DynamicImage> {
    let embedded = embedded_thumbnail(exif)?;
    match img::load_from_memory(embedded) {
        Ok(image) if image.width().max(image.height()) >= MIN_EMBEDDED_SIZE => Some(image),
        Ok(_) => None,
        Err(error) => {
            log::debug!("Unusable embedded preview: {error}");
            None
        }
    }
}

fn encode(image: DynamicImage, exif: Option<&exif::Exif>) -> Result<Vec<u8>> {
    /* Applied after scaling down, so that the rotation works on the smaller image */
    let image = orient(fit(image), exif.map_or(1, orientation));

    let mut thumbnail = vec![];
    JpegEncoder::new_with_quality(&mut thumbnail, JPEG_QUALITY)
        /* JPEG has no alpha channel, and a PNG screenshot in the album would otherwise fail here */
        .encode_image(&image.to_rgb8())?;
    Ok(thumbnail)
}

fn decode(photo: &[u8]) -> Result<DynamicImage> {
    img::load_from_memory(photo)
}

/// The preview a camera or phone stored in the file, if it left one
fn embedded_thumbnail(exif: &exif::Exif) -> Option<&[u8]> {
    let offset = integer(exif.get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)?)?;
    let length =
        integer(exif.get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL)?)?;
    /* The offset is counted from the start of the TIFF structure, which is what buf() hands out */
    exif.buf().get(offset..offset.checked_add(length)?)
}

fn integer(field: &exif::Field) -> Option<usize> {
    field.value.get_uint(0).map(|value| value as usize)
}

/// How the photo has to be turned to be the right way up. `image` does not do this by itself, and
/// a grid of forty tiles is where a photo lying on its side is impossible to miss.
fn orientation(exif: &exif::Exif) -> u16 {
    exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|field| field.value.get_uint(0))
        .unwrap_or(1) as u16
}

fn orient(image: DynamicImage, orientation: u16) -> DynamicImage {
    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        /* 1, and anything a file claims that is not one of these */
        _ => image,
    }
}

/// Scales a photo down to [THUMBNAIL_SIZE], keeping its proportions. Never scales up: an embedded
/// preview is what it is, and blowing it up would only cost bytes.
fn fit(image: DynamicImage) -> DynamicImage {
    let (width, height) = (image.width(), image.height());
    let longest = width.max(height);
    if longest <= THUMBNAIL_SIZE || longest == 0 {
        return image;
    }
    let scale = f64::from(THUMBNAIL_SIZE) / f64::from(longest);
    let scaled = |edge: u32| (f64::from(edge) * scale).round().max(1.0) as u32;
    Framed::resize(&image, scaled(width), scaled(height))
}

#[cfg(test)]
#[cfg(not(target_os = "windows"))]
pub(crate) const TEST_JPEG: &[u8] = include_bytes!("../../assets/test_loading.jpeg");
#[cfg(test)]
#[cfg(target_os = "windows")]
pub(crate) const TEST_JPEG: &[u8] = include_bytes!("..\\..\\assets\\test_loading.jpeg");

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    fn decoded(thumbnail: &[u8]) -> DynamicImage {
        img::load_from_memory(thumbnail).expect("a rendered preview is a readable JPEG")
    }

    #[test]
    fn render_produces_a_jpeg_within_the_thumbnail_size() {
        let thumbnail = render(TEST_JPEG).unwrap();

        let image = decoded(&thumbnail);
        assert!(
            image.width().max(image.height()) <= THUMBNAIL_SIZE,
            "was {}x{}",
            image.width(),
            image.height()
        );
    }

    #[test]
    fn render_keeps_the_proportions_of_the_photo() {
        let original = img::load_from_memory(TEST_JPEG).unwrap();
        let expected = f64::from(original.width()) / f64::from(original.height());

        let image = decoded(&render(TEST_JPEG).unwrap());

        let actual = f64::from(image.width()) / f64::from(image.height());
        assert!(
            (expected - actual).abs() < 0.05,
            "was {actual}, expected {expected}"
        );
    }

    #[test]
    fn render_reports_bytes_that_are_not_an_image() {
        assert!(render(b"this is not a photo").is_err());
    }

    #[test]
    fn fit_does_not_scale_a_small_image_up() {
        let small = DynamicImage::new_rgb8(64, 48);

        let fitted = fit(small);

        assert_eq!((fitted.width(), fitted.height()), (64, 48));
    }

    #[test]
    fn orient_turns_a_photo_that_was_taken_sideways() {
        let landscape = DynamicImage::new_rgb8(40, 20);

        /* 6 means the camera was rotated 90 degrees clockwise */
        let upright = orient(landscape, 6);

        assert_eq!((upright.width(), upright.height()), (20, 40));
    }

    #[test]
    fn orient_leaves_a_photo_alone_when_the_file_says_nothing_usable() {
        for orientation in [1, 0, 9, 42] {
            let image = orient(DynamicImage::new_rgb8(40, 20), orientation);

            assert_eq!((image.width(), image.height()), (40, 20));
        }
    }

    /* Cache */

    /// A JPEG carrying an embedded preview, built rather than stored: all that matters is that a
    /// preview is present and large enough, and making one keeps a binary fixture out of the repo.
    fn jpeg_with_embedded_preview() -> Vec<u8> {
        let preview = {
            let image = DynamicImage::new_rgb8(MIN_EMBEDDED_SIZE, MIN_EMBEDDED_SIZE);
            let mut bytes = vec![];
            JpegEncoder::new_with_quality(&mut bytes, 80)
                .encode_image(&image.to_rgb8())
                .expect("the preview has to encode");
            bytes
        };

        /* A TIFF structure holding an empty IFD0 and an IFD1 whose two tags say where the preview
         * is. Offsets are counted from the start of this structure. */
        let mut tiff = b"II\x2a\x00".to_vec();
        tiff.extend_from_slice(&8u32.to_le_bytes());
        let ifd1_offset: u32 = 8 + 2 + 4;
        tiff.extend_from_slice(&0u16.to_le_bytes());
        tiff.extend_from_slice(&ifd1_offset.to_le_bytes());

        let entries: u16 = 2;
        let preview_offset = ifd1_offset + 2 + u32::from(entries) * 12 + 4;
        tiff.extend_from_slice(&entries.to_le_bytes());
        for (tag, value) in [
            (0x0201u16, preview_offset),
            (0x0202u16, preview.len() as u32),
        ] {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&4u16.to_le_bytes()); /* LONG */
            tiff.extend_from_slice(&1u32.to_le_bytes());
            tiff.extend_from_slice(&value.to_le_bytes());
        }
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(&preview);

        let mut app1 = b"Exif\x00\x00".to_vec();
        app1.extend_from_slice(&tiff);

        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend_from_slice(&[0xFF, 0xE1]);
        jpeg.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&app1);
        /* Deliberately no image data: nothing on this path ever decodes the photo itself, and a
         * file that cannot be decoded proves it */
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        jpeg
    }

    #[test]
    fn a_preview_is_found_in_the_start_of_a_photo() {
        let photo = jpeg_with_embedded_preview();

        let thumbnail = render_embedded(&photo);

        assert!(
            thumbnail.is_some(),
            "the preview is right there in the bytes given"
        );
        assert!(img::load_from_memory(&thumbnail.unwrap()).is_ok());
    }

    #[test]
    fn a_photo_without_a_preview_reports_that_rather_than_decoding() {
        assert!(
            render_embedded(TEST_JPEG).is_none(),
            "this path must never fall back to decoding; the caller decides"
        );
    }

    /// The point of the whole arrangement: when the start of the file holds a usable preview, the
    /// rest of it is never fetched. That is the difference between a gallery tile costing a
    /// hundred kilobytes and costing several megabytes.
    #[test]
    fn a_photo_with_a_preview_is_not_downloaded_in_full() {
        let thumbnails = Thumbnails::new(1024 * 1024);
        let photo = Bytes::from(jpeg_with_embedded_preview());
        let wholes = AtomicUsize::new(0);

        let result = thumbnails.get(
            "a.jpg",
            || Ok(photo.clone()),
            || {
                wholes.fetch_add(1, Ordering::Relaxed);
                Ok(photo.clone())
            },
        );

        assert!(result.is_ok());
        assert_eq!(
            wholes.load(Ordering::Relaxed),
            0,
            "the preview was there, so the rest of the photo must not have been fetched"
        );
    }

    /// Reading part of a file asks more of a server than reading all of it, and one of them
    /// answered an abort in a way that left the session confused. A tile that cannot take the
    /// quick path has to fall back, not break.
    #[test]
    fn a_failed_head_read_falls_back_to_the_whole_photo() {
        let thumbnails = Thumbnails::new(1024 * 1024);
        let wholes = AtomicUsize::new(0);

        let result = thumbnails.get(
            "a.jpg",
            || Err(anyhow::anyhow!("the server did not like being interrupted")),
            || {
                wholes.fetch_add(1, Ordering::Relaxed);
                Ok(Bytes::from_static(TEST_JPEG))
            },
        );

        assert!(result.is_ok(), "the thumbnail still has to be produced");
        assert_eq!(wholes.load(Ordering::Relaxed), 1, "by the ordinary route");
    }

    #[test]
    fn a_photo_without_a_preview_is_downloaded_in_full() {
        let thumbnails = Thumbnails::new(1024 * 1024);
        let heads = AtomicUsize::new(0);
        let wholes = AtomicUsize::new(0);

        thumbnails
            .get(
                "a.jpg",
                || {
                    heads.fetch_add(1, Ordering::Relaxed);
                    Ok(Bytes::from_static(TEST_JPEG))
                },
                || {
                    wholes.fetch_add(1, Ordering::Relaxed);
                    Ok(Bytes::from_static(TEST_JPEG))
                },
            )
            .unwrap();

        assert_eq!(
            heads.load(Ordering::Relaxed),
            1,
            "the start is always tried"
        );
        assert_eq!(wholes.load(Ordering::Relaxed), 1, "and then the whole file");
    }

    #[test]
    fn cache_returns_a_stored_thumbnail_without_loading_it_again() {
        let thumbnails = Thumbnails::new(1024 * 1024);
        let loads = AtomicUsize::new(0);
        let count = || {
            loads.fetch_add(1, Ordering::Relaxed);
            Ok(Bytes::from_static(TEST_JPEG))
        };

        let first = thumbnails.get("a.jpg", count, count).unwrap();
        let before = loads.load(Ordering::Relaxed);
        let second = thumbnails.get("a.jpg", count, count).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            loads.load(Ordering::Relaxed),
            before,
            "the second request downloaded something instead of using the cache"
        );
    }

    #[test]
    fn cache_renders_again_after_a_photo_was_forgotten() {
        let thumbnails = Thumbnails::new(1024 * 1024);
        let loads = AtomicUsize::new(0);
        let count = || {
            loads.fetch_add(1, Ordering::Relaxed);
            Ok(Bytes::from_static(TEST_JPEG))
        };

        thumbnails.get("a.jpg", count, count).unwrap();
        let before = loads.load(Ordering::Relaxed);
        thumbnails.forget("a.jpg");
        thumbnails.get("a.jpg", count, count).unwrap();

        assert!(
            loads.load(Ordering::Relaxed) > before,
            "a forgotten photo has to be fetched again"
        );
    }

    #[test]
    fn cache_evicts_the_least_recently_used_entry_when_the_budget_is_exceeded() {
        let mut cache = Cache::new(10);
        cache.insert("a".to_string(), Bytes::from_static(b"12345"));
        cache.insert("b".to_string(), Bytes::from_static(b"12345"));
        /* Makes "a" the more recently used of the two */
        cache.take_recent("a");

        cache.insert("c".to_string(), Bytes::from_static(b"12345"));

        assert!(cache.take_recent("b").is_none(), "b was used longest ago");
        assert!(cache.take_recent("a").is_some());
        assert!(cache.take_recent("c").is_some());
    }

    #[test]
    fn cache_does_not_store_a_thumbnail_larger_than_the_whole_budget() {
        let mut cache = Cache::new(4);
        cache.insert("a".to_string(), Bytes::from_static(b"12"));

        cache.insert("big".to_string(), Bytes::from_static(b"1234567890"));

        assert!(cache.take_recent("big").is_none());
        assert!(
            cache.take_recent("a").is_some(),
            "storing what does not fit must not empty the cache"
        );
    }

    #[test]
    fn cache_replaces_an_entry_without_counting_it_twice() {
        let mut cache = Cache::new(10);
        cache.insert("a".to_string(), Bytes::from_static(b"12345"));
        cache.insert("a".to_string(), Bytes::from_static(b"12345"));

        assert_eq!(cache.used, 5);
    }

    /* Kept on disk */

    fn temporary_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "syno-photo-frame-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("a temporary directory has to be creatable");
        directory
    }

    /// The reason the disk cache exists: a frame restarts, and the photos that have to be fetched
    /// whole cost about a second each. Paying that once rather than once per restart is the point.
    #[test]
    fn a_thumbnail_survives_a_restart() {
        let directory = temporary_directory("restart");
        let first = Thumbnails::new(1024 * 1024).with_disk_cache(directory.clone(), 1024 * 1024);
        first
            .get(
                "holiday/a.jpg",
                || Ok(Bytes::from_static(TEST_JPEG)),
                || Ok(Bytes::from_static(TEST_JPEG)),
            )
            .unwrap();

        /* A second instance over the same directory is what a restart looks like */
        let second = Thumbnails::new(1024 * 1024).with_disk_cache(directory.clone(), 1024 * 1024);
        let loads = AtomicUsize::new(0);
        let count = || {
            loads.fetch_add(1, Ordering::Relaxed);
            Ok(Bytes::from_static(TEST_JPEG))
        };
        let thumbnail = second.get("holiday/a.jpg", count, count).unwrap();

        assert!(!thumbnail.is_empty());
        assert_eq!(
            loads.load(Ordering::Relaxed),
            0,
            "it was on disk, so nothing should have been downloaded"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn forgetting_a_photo_removes_its_file() {
        let directory = temporary_directory("forget");
        let thumbnails =
            Thumbnails::new(1024 * 1024).with_disk_cache(directory.clone(), 1024 * 1024);
        let load = || Ok(Bytes::from_static(TEST_JPEG));
        thumbnails.get("a.jpg", load, load).unwrap();
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);

        thumbnails.forget("a.jpg");

        assert_eq!(
            fs::read_dir(&directory).unwrap().count(),
            0,
            "a deleted photo must not leave a preview behind to be served for the next one that
             takes its name"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn forgetting_a_directory_removes_what_was_under_it() {
        let directory = temporary_directory("forget-under");
        let thumbnails =
            Thumbnails::new(1024 * 1024).with_disk_cache(directory.clone(), 1024 * 1024);
        let load = || Ok(Bytes::from_static(TEST_JPEG));
        thumbnails.get("summer/a.jpg", load, load).unwrap();
        thumbnails.get("summer/beach/b.jpg", load, load).unwrap();
        thumbnails.get("summertime/c.jpg", load, load).unwrap();

        thumbnails.forget_under("summer");

        let left: Vec<String> = fs::read_dir(&directory)
            .unwrap()
            .flatten()
            .filter_map(|entry| DiskCache::key_of(&entry.path()))
            .collect();
        assert_eq!(
            left,
            vec!["summertime/c.jpg".to_string()],
            "only the directory asked for, and the prefix has to end at a boundary"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    /// A path can be longer than a file name is allowed to be. Such a photo is simply not kept on
    /// disk; it is a photo like any other, only slower.
    #[test]
    fn a_path_too_long_for_a_file_name_is_not_kept_on_disk() {
        let directory = temporary_directory("long");
        let disk = DiskCache::new(directory.clone(), 1024 * 1024);
        let long = format!("{}.jpg", "a".repeat(LONGEST_FILE_NAME));

        assert!(disk.file_for("short.jpg").is_some());
        assert!(disk.file_for(&long).is_none());

        disk.write(&long, b"something");
        assert_eq!(
            fs::read_dir(&directory).unwrap().count(),
            0,
            "nothing should have been written"
        );
        assert!(disk.read(&long).is_none());
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn pruning_brings_the_cache_back_within_its_budget() {
        let directory = temporary_directory("prune");
        let disk = DiskCache::new(directory.clone(), 100);
        for name in ["a.jpg", "b.jpg", "c.jpg"] {
            disk.write(name, &[0u8; 60]);
            /* The oldest go first, and the timestamps have to differ for that to mean anything */
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        disk.prune();

        let total: u64 = fs::read_dir(&directory)
            .unwrap()
            .flatten()
            .filter_map(|entry| entry.metadata().ok())
            .map(|data| data.len())
            .sum();
        assert!(total <= 100, "still {total} bytes, over the budget of 100");
        assert!(
            disk.read("c.jpg").is_some(),
            "the newest has to be the one that survives"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn forget_under_drops_every_preview_below_the_directory() {
        let thumbnails = Thumbnails::new(1024 * 1024);
        let load = || Ok(Bytes::from_static(TEST_JPEG));
        thumbnails.get("summer/a.jpg", load, load).unwrap();
        thumbnails.get("summer/beach/b.jpg", load, load).unwrap();
        thumbnails.get("winter/c.jpg", load, load).unwrap();

        thumbnails.forget_under("summer");

        let mut cache = thumbnails.lock();
        assert!(cache.take_recent("summer/a.jpg").is_none());
        assert!(cache.take_recent("summer/beach/b.jpg").is_none());
        assert!(
            cache.take_recent("winter/c.jpg").is_some(),
            "another directory must not be touched"
        );
    }

    #[test]
    fn forget_under_leaves_a_directory_whose_name_only_starts_the_same() {
        let mut cache = Cache::new(100);
        cache.insert("summer/a.jpg".to_string(), Bytes::from_static(b"12345"));
        cache.insert("summertime/b.jpg".to_string(), Bytes::from_static(b"12345"));

        cache.remove_under("summer/");

        assert!(cache.take_recent("summer/a.jpg").is_none());
        assert!(
            cache.take_recent("summertime/b.jpg").is_some(),
            "the prefix has to end at a directory boundary"
        );
    }

    #[test]
    fn forget_under_gives_the_budget_back() {
        let mut cache = Cache::new(100);
        cache.insert("summer/a.jpg".to_string(), Bytes::from_static(b"12345"));
        cache.insert("summer/b.jpg".to_string(), Bytes::from_static(b"12345"));
        cache.insert("winter/c.jpg".to_string(), Bytes::from_static(b"12345"));

        cache.remove_under("summer/");

        assert_eq!(cache.used, 5, "the two dropped previews were still counted");
    }
}
