use anyhow::Result;
use mockall::mock;
use serde::de::DeserializeOwned;
use std::time::Duration;
use syno_api::{
    dto::ApiResponse,
    foto::browse::item::dto::Item,
    foto::browse::item::dto::{Additional, Address, Thumbnail},
};

use crate::http::{self, CookieStore, Jar, MockHttpResponse, Query, StatusCode, Url};

mock! {
    pub HttpClient {}

    impl http::HttpClient for HttpClient {
        type Response = MockHttpResponse;

        fn post<'a>(
            &self,
            url: &str,
            form: &[(&'a str, &'a str)],
            query: Query<'a>,
            header: Option<(&'a str, &'a str)>,
        ) -> Result<MockHttpResponse>;

        fn post_json<'a>(
            &self,
            url: &str,
            query: &[(&'a str, &'a str)],
            json: &serde_json::Value,
        ) -> Result<MockHttpResponse>;

        fn get<'a>(&self, url: &str, query: &[(&'a str, &'a str)]) -> Result<MockHttpResponse>;
    }
}

/// A backend standing in for a real server, for testing [crate::mirror].
///
/// A hand-written fake rather than a mockall mock because [crate::api_client::ApiClient::get_exif]
/// returns a `-> Result<impl Metadata>`, which mockall cannot express. The mocks elsewhere in this
/// crate sit on the transport traits, which have no such return.
///
/// It says the things a mirror has to cope with: the album changed, the listing failed, the
/// transfer came back short, this photo always fails.
pub mod api {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use anyhow::{Result, anyhow, bail};
    use bytes::Bytes;
    use chrono::{DateTime, TimeZone, Utc};

    use crate::{
        api_client::{ApiClient, LoginError, PhotoFacts, SortBy},
        cli::SourceSize,
        metadata::{Location, Metadata},
    };

    /// A real, decodable JPEG, so that the mirror's "did this arrive whole" check sees a photo
    pub const PHOTO: &[u8] = include_bytes!("../../assets/test_loading.jpeg");

    #[derive(Clone, Debug)]
    pub struct FakePhoto {
        pub key: String,
        pub fingerprint: Option<String>,
    }

    impl FakePhoto {
        pub fn new(key: &str) -> Self {
            Self {
                key: key.to_string(),
                fingerprint: Some("v1".to_string()),
            }
        }

        pub fn with_fingerprint(mut self, fingerprint: &str) -> Self {
            self.fingerprint = Some(fingerprint.to_string());
            self
        }
    }

    #[derive(Default)]
    pub struct FakeApiClient {
        album: Mutex<Vec<FakePhoto>>,
        /// Photos whose download comes back as these bytes instead of a whole photo
        broken: Mutex<Vec<(String, Bytes)>>,
        pub listing_fails: AtomicBool,
        pub login_fails: AtomicBool,
        pub downloads: AtomicUsize,
        pub listings: AtomicUsize,
    }

    impl FakeApiClient {
        pub fn with_album(keys: &[&str]) -> Self {
            let client = Self::default();
            client.set_album(keys);
            client
        }

        pub fn set_album(&self, keys: &[&str]) {
            *self.album.lock().unwrap() = keys.iter().map(|key| FakePhoto::new(key)).collect();
        }

        pub fn set_photos(&self, photos: Vec<FakePhoto>) {
            *self.album.lock().unwrap() = photos;
        }

        /// Makes one photo come back as something other than a whole image
        pub fn breaks(&self, key: &str, bytes: &'static [u8]) {
            self.broken
                .lock()
                .unwrap()
                .push((key.to_string(), Bytes::from_static(bytes)));
        }

        pub fn downloads(&self) -> usize {
            self.downloads.load(Ordering::Relaxed)
        }

        pub fn listings(&self) -> usize {
            self.listings.load(Ordering::Relaxed)
        }
    }

    pub struct FakeMetadata;

    impl Metadata for FakeMetadata {
        fn date(&self) -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap()
        }

        fn location(&self) -> Location {
            Location::new(Some("Somewhere".to_string()), None)
        }
    }

    impl ApiClient for FakeApiClient {
        type Photo = FakePhoto;

        fn is_logged_in(&self) -> bool {
            !self.login_fails.load(Ordering::Relaxed)
        }

        fn login(&self) -> Result<(), LoginError> {
            if self.login_fails.load(Ordering::Relaxed) {
                Err(LoginError(anyhow!("the server refused the login")))
            } else {
                Ok(())
            }
        }

        fn get_photo_metadata(&self, _: SortBy) -> Result<Vec<Self::Photo>> {
            self.listings.fetch_add(1, Ordering::Relaxed);
            if self.listing_fails.load(Ordering::Relaxed) {
                /* An error, never an empty list. The mirror's deletion policy rests on this
                 * distinction, so the fake has to honour it the way a backend does. */
                bail!("the server did not answer")
            }
            Ok(self.album.lock().unwrap().clone())
        }

        fn get_exif(&self, _: &Self::Photo) -> Result<impl Metadata> {
            Ok(FakeMetadata)
        }

        fn get_photo_bytes(&self, photo: &Self::Photo, _: SourceSize) -> Result<Bytes> {
            self.downloads.fetch_add(1, Ordering::Relaxed);
            if let Some((_, bytes)) = self
                .broken
                .lock()
                .unwrap()
                .iter()
                .find(|(key, _)| *key == photo.key)
            {
                return Ok(bytes.clone());
            }
            if !self
                .album
                .lock()
                .unwrap()
                .iter()
                .any(|known| known.key == photo.key)
            {
                bail!("{} is not in the album any more", photo.key)
            }
            Ok(Bytes::from_static(PHOTO))
        }

        fn photo_facts(&self, photo: &Self::Photo) -> PhotoFacts {
            PhotoFacts {
                key: photo.key.clone(),
                fingerprint: photo.fingerprint.clone(),
            }
        }
    }
}

pub mod rand {
    use crate::rand::Random;
    use std::cell::RefCell;
    use std::ops::Range;

    #[derive(Debug, Default)]
    pub struct FakeRandom {
        random_sequence: RefCell<Vec<u32>>,
        shuffle_swap_sequence: Vec<(usize, usize)>,
    }

    impl FakeRandom {
        pub fn with_random_sequence(self, sequence: Vec<u32>) -> Self {
            self.random_sequence.replace(sequence);
            self.random_sequence.borrow_mut().reverse();
            self
        }

        pub fn with_shuffle_result(mut self, swaps: Vec<(usize, usize)>) -> Self {
            self.shuffle_swap_sequence = swaps;
            self
        }
    }

    impl Random for FakeRandom {
        fn random_range(&self, _: Range<u32>) -> u32 {
            self.random_sequence
                .borrow_mut()
                .pop()
                .expect("should not be empty")
        }

        fn shuffle<T>(&self, slice: &mut [T]) {
            self.shuffle_swap_sequence
                .iter()
                .for_each(|s| slice.swap(s.0, s.1));
        }
    }
}

pub fn fake_sleep(_: Duration) {}

/// When `is_logged_in_to_url` is set to Some value, cookie store will simulate logged in state
pub fn new_cookie_store(is_logged_in_to_url: Option<&str>) -> impl CookieStore {
    let cookie_store = Jar::default();
    if let Some(url) = is_logged_in_to_url {
        cookie_store.add_cookie_str("sharing_id=FakeSharingId", &Url::parse(url).unwrap());
    }
    cookie_store
}

pub fn new_ok_response() -> MockHttpResponse {
    let mut response = MockHttpResponse::new();
    response.expect_status().return_const(StatusCode::OK);
    response
}

pub fn new_success_response_with_json<T: DeserializeOwned + Send + 'static>(
    data: T,
) -> MockHttpResponse {
    let mut response = new_ok_response();
    response.expect_json::<ApiResponse<T>>().return_once(|| {
        Ok(ApiResponse {
            success: true,
            error: None,
            data: Some(data),
        })
    });
    response
}

pub fn new_photo_dto(id: u32, cache_key: &str) -> Item {
    Item {
        id,
        time: 1760788393,
        additional: Some(Additional {
            thumbnail: Some(Thumbnail {
                cache_key: cache_key.to_string(),
                unit_id: id,
            }),
            address: Some(Address {
                city: "Copenhagen".to_string(),
                country: "Denmark".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn is_login_form(form: &[(&str, &str)], sharing_id: &str) -> bool {
    form.eq(&[
        ("api", "SYNO.Core.Sharing.Login"),
        ("method", "login"),
        ("version", "1"),
        ("sharing_id", sharing_id),
        ("password", ""),
    ])
}

pub fn is_list_form(form: &[(&str, &str)]) -> bool {
    form.eq(&[
        ("api", syno_api::foto::browse::item::API),
        ("method", "list"),
        ("version", "4"),
        ("additional", "[\"thumbnail\", \"address\"]"),
        ("offset", "0"),
        ("limit", "5000"),
        ("sort_by", "takentime"),
        ("sort_direction", "asc"),
    ])
}

pub fn is_get_photo_form(
    form: &[(&str, &str)],
    sharing_id: &str,
    photo_id: &str,
    cache_key: &str,
    size: &str,
) -> bool {
    form.eq(&[
        ("type", "unit"),
        ("id", photo_id),
        ("cache_key", cache_key),
        ("_sharing_id", sharing_id),
        ("size", size),
    ])
}
