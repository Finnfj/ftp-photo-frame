use std::{
    error::Error,
    fmt::{Display, Formatter},
};

use bytes::Bytes;
use ftp::FtpStream;

use crate::{
    cli::{Order, SourceSize},
    // error::ErrorToString,
    http::Url,
    Random,
};

#[derive(Clone, Copy, Debug)]
pub enum SortBy {
    TakenTime,
    FileName,
}


/// Holds the slideshow state and queries API to fetch photos.
#[derive(Debug)]
pub struct Slideshow<'a> {
    ftp_server: &'a Url,
    user: &'a Option<String>,
    password: &'a Option<String>,
    /// Indices of photos in an album in reverse order (so we can pop them off easily)
    photo_display_sequence: Vec<u32>,
    order: Order,
    random_start: bool,
    source_size: SourceSize,
}

#[derive(Debug)]
pub enum SlideshowError {
    Other(String),
}

impl<'a> Slideshow<'a> {
    pub fn build(ftp_server: &'a Url, user: &'a Option<String>) -> Result<Slideshow<'a>, String> {
        Ok(Slideshow {
            ftp_server,
            user,
            password: &None,
            photo_display_sequence: vec![],
            order: Order::ByDate,
            random_start: false,
            source_size: SourceSize::L,
        })
    }

    pub fn with_password(mut self, password: &'a Option<String>) -> Self {
        self.password = password;
        self
    }

    pub fn with_ordering(mut self, order: Order) -> Self {
        self.order = order;
        self
    }

    pub fn with_random_start(mut self, random_start: bool) -> Self {
        self.random_start = random_start;
        self
    }

    pub fn with_source_size(mut self, size: SourceSize) -> Self {
        self.source_size = size;
        self
    }

    fn get_photos_count(&self) -> u32 {
        // Create a connection to FTP server
        let ftp_connect = self.ftp_server.host_str().unwrap();
        let mut ftp_stream = FtpStream::connect(format!("{}:21", ftp_connect)).unwrap();
        let _ = ftp_stream.login(self.user.clone().unwrap().as_str(), self.password.clone().unwrap().as_str()).unwrap();

        
        // Change into a new directory, relative to the one we are currently in.
        let _ = ftp_stream.cwd(self.ftp_server.path()).unwrap();

        // Fetch list of Photos
        let photos = ftp_stream.nlst(None).unwrap();

        // Terminate the connection to the server.
        let _ = ftp_stream.quit();
        photos.len() as u32
    }

    pub fn get_photo(&mut self, photo_index: u32) -> Result<Bytes, ()> {
        // Create a connection to an FTP server and authenticate to it.
        let ftp_connect = self.ftp_server.host_str().unwrap();
        let mut ftp_stream = FtpStream::connect(format!("{}:21", ftp_connect)).unwrap();
        let _ = ftp_stream.login(self.user.clone().unwrap().as_str(), self.password.clone().unwrap().as_str()).unwrap();

        
        // Change into a new directory, relative to the one we are currently in.
        let _ = ftp_stream.cwd(self.ftp_server.path()).unwrap();

        // Fetch list of Photos
        let photos = ftp_stream.nlst(None).unwrap();

        // Retrieve (GET) a file from the FTP server in the current working directory.
        let remote_file = Bytes::from(ftp_stream.simple_retr(photos.get(photo_index as usize).unwrap()).unwrap().into_inner());


        // Terminate the connection to the server.
        let _ = ftp_stream.quit();
        Ok(remote_file)
    }

    pub fn get_next_photo(
        &mut self,
        random: Random,
    ) -> Result<Bytes, SlideshowError> {
        loop {
            if self.slideshow_ended() {
                self.initialize(random)?;
            }

            let photo_index = self
                .photo_display_sequence
                .pop()
                .expect("photos should not be empty");

            let photo_bytes_result = self.get_photo(photo_index);
            match photo_bytes_result {
                Ok(photo_bytes) => break Ok(photo_bytes),
                Err(_) => { 
                    /* Photos were removed from the album since we fetched its item_count. Reinitialize */
                    self.photo_display_sequence.clear();
                    continue; 
                },
            }
        }
    }

    fn slideshow_ended(&self) -> bool {
        self.photo_display_sequence.is_empty()
    }

    fn initialize(
        &mut self,
        (rand_gen_range, rand_shuffle): Random,
    ) -> Result<(), String> {
        assert!(
            self.photo_display_sequence.is_empty(),
            "already initialized"
        );
        let item_count = self.get_photos_count();
        if item_count < 1 {
            return Err("Album is empty".to_string());
        }
        self.photo_display_sequence.reserve(item_count as usize);
        let photos_range = 0..item_count;
        match self.order {
            Order::ByDate | Order::ByName => {
                if self.random_start {
                    self.photo_display_sequence.extend(
                        photos_range
                            .skip(rand_gen_range(0..item_count) as usize)
                            .rev(),
                    );
                    /* RandomStart is only used when slideshow starts, and afterward continues in normal order */
                    self.random_start = false;
                } else {
                    self.photo_display_sequence.extend(photos_range.rev());
                }
            }
            Order::Random => {
                self.photo_display_sequence.extend(photos_range);
                rand_shuffle(&mut self.photo_display_sequence)
            }
        }

        Ok(())
    }
}

impl From<Order> for SortBy {
    fn from(value: Order) -> Self {
        match value {
            /* Random is not an option in the API. Randomization is implemented client-side and
             * essentially makes the sort_by query parameter irrelevant. */
            Order::ByDate | Order::Random => SortBy::TakenTime,
            Order::ByName => SortBy::FileName,
        }
    }
}

impl Error for SlideshowError {}

impl Display for SlideshowError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SlideshowError::Other(error) => write!(f, "{error}"),
        }
    }
}

impl From<String> for SlideshowError {
    fn from(value: String) -> Self {
        SlideshowError::Other(value)
    }
}

// /// These tests cover both `slideshow` and `api_photos` modules
// #[cfg(test)]
// mod tests {
//     use super::*;
//     use crate::{
//         api_photos::dto,
//         http::{Jar, MockResponse},
//         test_helpers::{self, MockClient},
//     };

//     #[test]
//     fn when_default_order_then_get_next_photo_starts_by_sending_login_request_and_fetches_first_photo(
//     ) {
//         /* Arrange */
//         const SHARE_LINK: &str = "http://fake.dsm.addr/aa/sharing/FakeSharingId";
//         const EXPECTED_API_URL: &str = "http://fake.dsm.addr/aa/sharing/webapi/entry.cgi";
//         let mut slideshow = new_slideshow(SHARE_LINK);
//         let mut client_mock = MockClient::new();
//         client_mock
//             .expect_post()
//             .withf(|url, form, _| {
//                 url == EXPECTED_API_URL && test_helpers::is_login_form(form, "FakeSharingId")
//             })
//             .return_once(|_, _, _| Ok(test_helpers::new_success_response_with_json(dto::Login {})));
//         const PHOTO_COUNT: u32 = 3;
//         client_mock
//             .expect_post()
//             .withf(|url, form, header| {
//                 url == EXPECTED_API_URL
//                     && test_helpers::is_get_count_form(form)
//                     && *header == Some(("X-SYNO-SHARING", "FakeSharingId"))
//             })
//             .return_once(|_, _, _| {
//                 Ok(test_helpers::new_success_response_with_json(dto::List {
//                     list: vec![Album {
//                         item_count: PHOTO_COUNT,
//                     }],
//                 }))
//             });
//         const FIRST_PHOTO_INDEX: u32 = 0;
//         const FIRST_PHOTO_ID: i32 = 1;
//         const FIRST_PHOTO_CACHE_KEY: &str = "photo1";
//         client_mock
//             .expect_post()
//             .withf(|url, form, header| {
//                 url == EXPECTED_API_URL
//                     && is_list_form(form, &FIRST_PHOTO_INDEX.to_string(), "1")
//                     && *header == Some(("X-SYNO-SHARING", "FakeSharingId"))
//             })
//             .return_once(|_, _, _| {
//                 Ok(test_helpers::new_success_response_with_json(dto::List {
//                     list: vec![test_helpers::new_photo_dto(
//                         FIRST_PHOTO_ID,
//                         FIRST_PHOTO_CACHE_KEY,
//                     )],
//                 }))
//             });
//         client_mock
//             .expect_get()
//             .withf(|url, query| {
//                 url == EXPECTED_API_URL
//                     && is_get_photo_query(
//                         query,
//                         &FIRST_PHOTO_ID.to_string(),
//                         "FakeSharingId",
//                         FIRST_PHOTO_CACHE_KEY,
//                         "xl",
//                     )
//             })
//             .return_once(|_, _| {
//                 let mut get_photo_response = test_helpers::new_ok_response();
//                 get_photo_response
//                     .expect_bytes()
//                     .return_once(|| Ok(Bytes::from_static(&[42, 1, 255, 50])));
//                 Ok(get_photo_response)
//             });

//         /* Act */
//         let result = slideshow.get_next_photo((&client_mock, &Jar::default()), DUMMY_RANDOM);

//         /* Assert */
//         assert!(result.is_ok());
//         assert_eq!(result.unwrap(), Bytes::from_static(&[42, 1, 255, 50]));

//         const EXPECTED_REMAINING_DISPLAY_SEQUENCE: [u32; 2] = [2, 1];
//         assert_eq!(
//             slideshow.photo_display_sequence,
//             EXPECTED_REMAINING_DISPLAY_SEQUENCE
//         );

//         client_mock.checkpoint();
//     }

//     #[test]
//     fn when_random_start_then_get_next_photo_starts_by_sending_login_request_and_fetches_random_photo(
//     ) {
//         /* Arrange */
//         const SHARE_LINK: &str = "http://fake.dsm.addr/aa/sharing/FakeSharingId";
//         let mut slideshow = new_slideshow(SHARE_LINK).with_random_start(true);
//         let mut client_mock = MockClient::new();
//         client_mock
//             .expect_post()
//             .withf(|_, form, _| test_helpers::is_login_form(form, "FakeSharingId"))
//             .return_once(|_, _, _| Ok(test_helpers::new_success_response_with_json(dto::Login {})));
//         const PHOTO_COUNT: u32 = 142;
//         client_mock
//             .expect_post()
//             .withf(|_, form, _| test_helpers::is_get_count_form(form))
//             .return_once(|_, _, _| {
//                 Ok(test_helpers::new_success_response_with_json(dto::List {
//                     list: vec![dto::Album {
//                         item_count: PHOTO_COUNT,
//                     }],
//                 }))
//             });
//         const FAKE_RANDOM_NUMBER: u32 = 42;
//         const RANDOM_PHOTO_ID: i32 = 43;
//         const RANDOM_PHOTO_CACHE_KEY: &str = "photo43";
//         client_mock
//             .expect_post()
//             .withf(|_, form, _| is_list_form(form, &FAKE_RANDOM_NUMBER.to_string(), "1"))
//             .return_once(|_, _, _| {
//                 Ok(test_helpers::new_success_response_with_json(dto::List {
//                     list: vec![test_helpers::new_photo_dto(
//                         RANDOM_PHOTO_ID,
//                         RANDOM_PHOTO_CACHE_KEY,
//                     )],
//                 }))
//             });
//         client_mock
//             .expect_get()
//             .withf(|_, query| {
//                 is_get_photo_query(
//                     query,
//                     &RANDOM_PHOTO_ID.to_string(),
//                     "FakeSharingId",
//                     RANDOM_PHOTO_CACHE_KEY,
//                     "xl",
//                 )
//             })
//             .return_once(|_, _| {
//                 let mut get_photo_response = test_helpers::new_ok_response();
//                 get_photo_response
//                     .expect_bytes()
//                     .return_once(|| Ok(Bytes::from_static(&[42, 1, 255, 50])));
//                 Ok(get_photo_response)
//             });

//         let random_mock: Random = (
//             |range| {
//                 assert_eq!(range, 0..PHOTO_COUNT);
//                 FAKE_RANDOM_NUMBER
//             },
//             |_| (),
//         );

//         /* Act */
//         let result = slideshow.get_next_photo((&client_mock, &Jar::default()), random_mock);

//         /* Assert */
//         assert!(result.is_ok());
//         client_mock.checkpoint();
//     }

//     #[test]
//     fn when_source_size_specified_then_get_next_photo_fetches_photo_of_specific_size() {
//         test_case(SourceSize::S, "sm");
//         test_case(SourceSize::M, "m");
//         test_case(SourceSize::L, "xl");

//         fn test_case(source_size: SourceSize, expected_size_param: &'static str) {
//             /* Arrange */
//             const SHARE_LINK: &str = "http://fake.dsm.addr/aa/sharing/FakeSharingId";
//             let mut slideshow = new_slideshow(SHARE_LINK).with_source_size(source_size);
//             let mut client_mock = MockClient::new();
//             client_mock
//                 .expect_post()
//                 .withf(|_, form, _| test_helpers::is_login_form(form, "FakeSharingId"))
//                 .return_once(|_, _, _| {
//                     Ok(test_helpers::new_success_response_with_json(dto::Login {}))
//                 });
//             const PHOTO_COUNT: u32 = 142;
//             client_mock
//                 .expect_post()
//                 .withf(|_, form, _| test_helpers::is_get_count_form(form))
//                 .return_once(|_, _, _| {
//                     Ok(test_helpers::new_success_response_with_json(dto::List {
//                         list: vec![dto::Album {
//                             item_count: PHOTO_COUNT,
//                         }],
//                     }))
//                 });
//             client_mock
//                 .expect_post()
//                 .withf(|_, form, _| is_list_form(form, "0", "1"))
//                 .return_once(|_, _, _| {
//                     Ok(test_helpers::new_success_response_with_json(dto::List {
//                         list: vec![test_helpers::new_photo_dto(43, "photo43")],
//                     }))
//                 });
//             client_mock
//                 .expect_get()
//                 .withf(move |_, query| {
//                     is_get_photo_query(query, "43", "FakeSharingId", "photo43", expected_size_param)
//                 })
//                 .return_once(|_, _| {
//                     let mut get_photo_response = test_helpers::new_ok_response();
//                     get_photo_response
//                         .expect_bytes()
//                         .return_once(|| Ok(Bytes::from_static(&[42, 1, 255, 50])));
//                     Ok(get_photo_response)
//                 });

//             /* Act */
//             let result = slideshow.get_next_photo((&client_mock, &Jar::default()), DUMMY_RANDOM);

//             /* Assert */
//             assert!(result.is_ok());
//             client_mock.checkpoint();
//         }
//     }

//     #[test]
//     fn get_next_photo_advances_to_next_photo() {
//         /* Arrange */
//         const SHARE_LINK: &str = "http://fake.dsm.addr/aa/sharing/FakeSharingId";
//         const EXPECTED_API_URL: &str = "http://fake.dsm.addr/aa/sharing/webapi/entry.cgi";
//         let mut slideshow = new_slideshow(SHARE_LINK);
//         const NEXT_PHOTO_INDEX: u32 = 2;
//         slideshow.photo_display_sequence = vec![3, NEXT_PHOTO_INDEX];
//         const NEXT_PHOTO_ID: i32 = 3;
//         const NEXT_PHOTO_CACHE_KEY: &str = "photo3";
//         let mut client_mock = MockClient::new();
//         client_mock
//             .expect_post()
//             .withf(|url, form, header| {
//                 url == "http://fake.dsm.addr/aa/sharing/webapi/entry.cgi"
//                     && is_list_form(form, &NEXT_PHOTO_INDEX.to_string(), "1")
//                     && *header == Some(("X-SYNO-SHARING", "FakeSharingId"))
//             })
//             .return_once(|_, _, _| {
//                 Ok(test_helpers::new_success_response_with_json(dto::List {
//                     list: vec![test_helpers::new_photo_dto(
//                         NEXT_PHOTO_ID,
//                         NEXT_PHOTO_CACHE_KEY,
//                     )],
//                 }))
//             });
//         client_mock
//             .expect_get()
//             .withf(|url, query| {
//                 url == "http://fake.dsm.addr/aa/sharing/webapi/entry.cgi"
//                     && is_get_photo_query(
//                         query,
//                         &NEXT_PHOTO_ID.to_string(),
//                         "FakeSharingId",
//                         NEXT_PHOTO_CACHE_KEY,
//                         "xl",
//                     )
//             })
//             .return_once(|_, _| {
//                 let mut get_photo_response = test_helpers::new_ok_response();
//                 get_photo_response
//                     .expect_bytes()
//                     .return_once(|| Ok(Bytes::from_static(&[])));
//                 Ok(get_photo_response)
//             });

//         /* Act */
//         let result = slideshow.get_next_photo(
//             (&client_mock, &logged_in_cookie_store(EXPECTED_API_URL)),
//             DUMMY_RANDOM,
//         );

//         /* Assert */
//         assert!(result.is_ok());
//         assert_eq!(slideshow.photo_display_sequence, vec![3]);
//     }

//     #[test]
//     fn get_next_photo_skips_to_next_photo_when_cached_dto_is_not_found_because_photo_was_removed_from_album(
//     ) {
//         /* Arrange */
//         const SHARE_LINK: &str = "http://fake.dsm.addr/aa/sharing/FakeSharingId";
//         const EXPECTED_API_URL: &str = "http://fake.dsm.addr/aa/sharing/webapi/entry.cgi";
//         let mut slideshow = new_slideshow(SHARE_LINK);
//         const NEXT_PHOTO_INDEX: u32 = 1;
//         const NEXT_NEXT_PHOTO_INDEX: u32 = 2;
//         slideshow.photo_display_sequence = vec![3, NEXT_NEXT_PHOTO_INDEX, NEXT_PHOTO_INDEX];
//         const NEXT_PHOTO_ID: i32 = 2;
//         const NEXT_PHOTO_CACHE_KEY: &str = "photo2";
//         let mut client_mock = MockClient::new();
//         client_mock
//             .expect_post()
//             .withf(|_, form, _| is_list_form(form, &NEXT_PHOTO_INDEX.to_string(), "1"))
//             .return_once(|_, _, _| {
//                 Ok(test_helpers::new_success_response_with_json(dto::List {
//                     list: vec![test_helpers::new_photo_dto(
//                         NEXT_PHOTO_ID,
//                         NEXT_PHOTO_CACHE_KEY,
//                     )],
//                 }))
//             });
//         client_mock
//             .expect_get()
//             .withf(|_, query| {
//                 is_get_photo_query(
//                     query,
//                     &NEXT_PHOTO_ID.to_string(),
//                     "FakeSharingId",
//                     NEXT_PHOTO_CACHE_KEY,
//                     "xl",
//                 )
//             })
//             .return_once(|_, _| {
//                 let mut not_found_response = MockResponse::new();
//                 not_found_response
//                     .expect_status()
//                     .returning(|| StatusCode::NOT_FOUND);
//                 Ok(not_found_response)
//             });
//         client_mock
//             .expect_post()
//             .withf(|_, form, _| is_list_form(form, &NEXT_NEXT_PHOTO_INDEX.to_string(), "1"))
//             .return_once(|_, _, _| {
//                 Ok(test_helpers::new_success_response_with_json(dto::List {
//                     list: vec![test_helpers::new_photo_dto(3, "photo3")],
//                 }))
//             });
//         const NEXT_NEXT_PHOTO_ID: i32 = 3;
//         const NEXT_NEXT_PHOTO_CACHE_KEY: &str = "photo3";
//         client_mock
//             .expect_get()
//             .withf(|_, query| {
//                 is_get_photo_query(
//                     query,
//                     &NEXT_NEXT_PHOTO_ID.to_string(),
//                     "FakeSharingId",
//                     NEXT_NEXT_PHOTO_CACHE_KEY,
//                     "xl",
//                 )
//             })
//             .return_once(|_, _| {
//                 let mut get_photo_response = test_helpers::new_ok_response();
//                 get_photo_response
//                     .expect_bytes()
//                     .return_once(|| Ok(Bytes::from_static(&[])));
//                 Ok(get_photo_response)
//             });

//         /* Act */
//         let result = slideshow.get_next_photo(
//             (&client_mock, &logged_in_cookie_store(EXPECTED_API_URL)),
//             DUMMY_RANDOM,
//         );

//         /* Assert */
//         assert!(result.is_ok());
//         assert_eq!(slideshow.photo_display_sequence, vec![3]);
//     }

//     #[test]
//     fn when_random_order_then_photo_display_sequence_is_shuffled() {
//         /* Arrange */
//         const SHARE_LINK: &str = "http://fake.dsm.addr/aa/sharing/FakeSharingId";
//         let mut slideshow = new_slideshow(SHARE_LINK).with_ordering(Order::Random);
//         let mut client_mock = MockClient::new();
//         client_mock
//             .expect_post()
//             .withf(|_, form, _| test_helpers::is_login_form(form, "FakeSharingId"))
//             .return_once(|_, _, _| Ok(test_helpers::new_success_response_with_json(dto::Login {})));
//         const PHOTO_COUNT: u32 = 5;
//         client_mock
//             .expect_post()
//             .withf(|_, form, _| test_helpers::is_get_count_form(form))
//             .return_once(|_, _, _| {
//                 Ok(test_helpers::new_success_response_with_json(dto::List {
//                     list: vec![dto::Album {
//                         item_count: PHOTO_COUNT,
//                     }],
//                 }))
//             });
//         const FIRST_PHOTO_INDEX: u32 = 3;
//         client_mock
//             .expect_post()
//             .withf(|_, form, _| is_list_form(form, &FIRST_PHOTO_INDEX.to_string(), "1"))
//             .return_once(|_, _, _| {
//                 Ok(test_helpers::new_success_response_with_json(dto::List {
//                     list: vec![test_helpers::new_photo_dto(4, "photo4")],
//                 }))
//             });
//         client_mock
//             .expect_get()
//             .withf(|_, query| is_get_photo_query(query, "4", "FakeSharingId", "photo4", "xl"))
//             .return_once(|_, _| {
//                 let mut get_photo_response = test_helpers::new_ok_response();
//                 get_photo_response
//                     .expect_bytes()
//                     .return_once(|| Ok(Bytes::from_static(&[42, 1, 255, 50])));
//                 Ok(get_photo_response)
//             });

//         let random_mock: Random = (
//             |_| 0,
//             |slice| {
//                 slice[0] = 5;
//                 slice[1] = 2;
//                 slice[2] = 4;
//                 slice[3] = 1;
//                 slice[4] = FIRST_PHOTO_INDEX;
//             },
//         );

//         /* Act */
//         let result = slideshow.get_next_photo((&client_mock, &Jar::default()), random_mock);

//         assert!(result.is_ok());
//         assert_eq!(slideshow.photo_display_sequence, vec![5, 2, 4, 1]);
//     }

//     /// Tests that when photos were removed, slideshow gets re-initialized when reaching the end of the album
//     #[test]
//     fn get_next_photo_reinitializes_when_display_sequence_is_shorter_than_photo_album() {
//         /* Arrange */
//         const SHARE_LINK: &str = "http://fake.dsm.addr/aa/sharing/FakeSharingId";
//         const EXPECTED_API_URL: &str = "http://fake.dsm.addr/aa/sharing/webapi/entry.cgi";
//         let mut slideshow = new_slideshow(SHARE_LINK);
//         const NEXT_PHOTO_INDEX: u32 = 3;
//         slideshow.photo_display_sequence = vec![5, 4, NEXT_PHOTO_INDEX];
//         let mut client_mock = MockClient::new();
//         client_mock
//             .expect_post()
//             .withf(|_, form, _| is_list_form(form, &NEXT_PHOTO_INDEX.to_string(), "1"))
//             .return_once(|_, _, _| {
//                 Ok(test_helpers::new_success_response_with_json(dto::List {
//                     list: Vec::<dto::Photo>::new(), // EMPTY
//                 }))
//             });
//         const NEW_PHOTO_COUNT: u32 = 3;
//         client_mock
//             .expect_post()
//             .withf(|_, form, _| test_helpers::is_get_count_form(form))
//             .return_once(|_, _, _| {
//                 Ok(test_helpers::new_success_response_with_json(dto::List {
//                     list: vec![dto::Album {
//                         item_count: NEW_PHOTO_COUNT,
//                     }],
//                 }))
//             });

//         const FIRST_PHOTO_INDEX: u32 = 0;
//         const FIRST_PHOTO_ID: i32 = 1;
//         const FIRST_PHOTO_CACHE_KEY: &str = "photo1";
//         client_mock
//             .expect_post()
//             .withf(|_, form, _| is_list_form(form, &FIRST_PHOTO_INDEX.to_string(), "1"))
//             .return_once(|_, _, _| {
//                 Ok(test_helpers::new_success_response_with_json(dto::List {
//                     list: vec![test_helpers::new_photo_dto(
//                         FIRST_PHOTO_ID,
//                         FIRST_PHOTO_CACHE_KEY,
//                     )],
//                 }))
//             });
//         client_mock
//             .expect_get()
//             .withf(|_, query| {
//                 is_get_photo_query(
//                     query,
//                     &FIRST_PHOTO_ID.to_string(),
//                     "FakeSharingId",
//                     FIRST_PHOTO_CACHE_KEY,
//                     "xl",
//                 )
//             })
//             .return_once(|_, _| {
//                 let mut get_photo_response = test_helpers::new_ok_response();
//                 get_photo_response
//                     .expect_bytes()
//                     .return_once(|| Ok(Bytes::from_static(&[])));
//                 Ok(get_photo_response)
//             });

//         /* Act */
//         let result = slideshow.get_next_photo(
//             (&client_mock, &logged_in_cookie_store(EXPECTED_API_URL)),
//             DUMMY_RANDOM,
//         );

//         /* Assert */
//         assert!(result.is_ok());
//         const EXPECTED_REINITIALIZED_DISPLAY_SEQUENCE: [u32; 2] = [2, 1];
//         assert_eq!(
//             slideshow.photo_display_sequence,
//             EXPECTED_REINITIALIZED_DISPLAY_SEQUENCE
//         );
//     }

//     const DUMMY_RANDOM: Random = (|_| 42, |_| ());

//     fn new_slideshow(share_link: &str) -> Slideshow {
//         let share_link = Url::parse(share_link).unwrap();

//         Slideshow::build(&share_link, ).unwrap()
//     }

//     fn is_list_form(form: &[(&str, &str)], offset: &str, limit: &str) -> bool {
//         form.eq(&[
//             ("api", "SYNO.Foto.Browse.Item"),
//             ("method", "list"),
//             ("version", "1"),
//             ("additional", "[\"thumbnail\"]"),
//             ("offset", offset),
//             ("limit", limit),
//             ("sort_by", "takentime"),
//             ("sort_direction", "asc"),
//         ])
//     }

//     fn is_get_photo_query(
//         query: &[(&str, &str)],
//         id: &str,
//         sharing_id: &str,
//         cache_key: &str,
//         size: &str,
//     ) -> bool {
//         query.eq(&[
//             ("api", "SYNO.Foto.Thumbnail"),
//             ("method", "get"),
//             ("version", "2"),
//             ("_sharing_id", sharing_id),
//             ("id", id),
//             ("cache_key", cache_key),
//             ("type", "unit"),
//             ("size", size),
//         ])
//     }

//     fn logged_in_cookie_store(url: &str) -> impl CookieStore {
//         test_helpers::new_cookie_store(Some(url))
//     }
// }
