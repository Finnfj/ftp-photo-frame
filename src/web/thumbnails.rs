//! Rendering and remembering the small previews the gallery shows.
//!
//! A frame is a Raspberry Pi that is already decoding and resizing a full-screen photo every few
//! seconds, so a gallery must not ask it to decode a 12 megapixel JPEG per tile. Two things keep
//! that from happening: the preview a camera or phone already embedded in the file is used when
//! there is one, and whatever was rendered is kept until the memory budget runs out.

use std::{
    collections::VecDeque,
    io::Cursor,
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
}

impl Thumbnails {
    pub fn new(budget: usize) -> Self {
        Self {
            cache: Mutex::new(Cache::new(budget)),
        }
    }

    /// The preview of a photo, rendering it if it has not been asked for recently.
    ///
    /// `load` fetches the photo itself and is only called on a miss, which is the expensive part:
    /// it is a download over FTP.
    pub fn get(&self, key: &str, load: impl FnOnce() -> Result<Bytes>) -> Result<Bytes> {
        if let Some(cached) = self.lock().take_recent(key) {
            return Ok(cached);
        }
        let thumbnail = Bytes::from(render(&load()?)?);
        self.lock().insert(key.to_string(), thumbnail.clone());
        Ok(thumbnail)
    }

    /// Drops the preview of a photo that is gone.
    ///
    /// Nothing else has to invalidate an entry: an upload never overwrites, it picks a free name,
    /// so the bytes behind a path do not change while the frame is running.
    pub fn forget(&self, key: &str) {
        self.lock().remove(key);
    }

    fn lock(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
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
}

/// Turns the bytes of a photo into the bytes of its preview
fn render(photo: &[u8]) -> Result<Vec<u8>> {
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(photo))
        .ok();

    let image = match exif.as_ref().and_then(embedded_thumbnail) {
        /* Free next to decoding the photo itself, which on a Pi costs a second or more */
        Some(embedded) => match img::load_from_memory(embedded) {
            Ok(image) if image.width().max(image.height()) >= MIN_EMBEDDED_SIZE => image,
            Ok(_) => decode(photo)?,
            Err(error) => {
                log::debug!("Unusable embedded preview: {error}");
                decode(photo)?
            }
        },
        None => decode(photo)?,
    };

    /* Applied after scaling down, so that the rotation works on the smaller image */
    let image = orient(fit(image), exif.as_ref().map_or(1, orientation));

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

    #[test]
    fn cache_returns_a_stored_thumbnail_without_loading_it_again() {
        let thumbnails = Thumbnails::new(1024 * 1024);
        let loads = AtomicUsize::new(0);
        let load = || {
            loads.fetch_add(1, Ordering::Relaxed);
            Ok(Bytes::from_static(TEST_JPEG))
        };

        let first = thumbnails.get("a.jpg", load).unwrap();
        let second = thumbnails.get("a.jpg", load).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            loads.load(Ordering::Relaxed),
            1,
            "the photo was downloaded twice"
        );
    }

    #[test]
    fn cache_renders_again_after_a_photo_was_forgotten() {
        let thumbnails = Thumbnails::new(1024 * 1024);
        let loads = AtomicUsize::new(0);
        let load = || {
            loads.fetch_add(1, Ordering::Relaxed);
            Ok(Bytes::from_static(TEST_JPEG))
        };

        thumbnails.get("a.jpg", load).unwrap();
        thumbnails.forget("a.jpg");
        thumbnails.get("a.jpg", load).unwrap();

        assert_eq!(loads.load(Ordering::Relaxed), 2);
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
}
