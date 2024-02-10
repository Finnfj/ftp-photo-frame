//! HTTP request-response handling

pub(crate) use bytes::Bytes;
pub use reqwest::{blocking::ClientBuilder, cookie::CookieStore};
pub(crate) use reqwest::{StatusCode, Url};

#[cfg(test)]
pub(crate) use reqwest::cookie::Jar;

use serde::de::DeserializeOwned;

use crate::error::ErrorToString;

/// Isolates [reqwest::blocking::Client] for testing
pub trait Client {
    type Response: Response;

    fn post(
        &self,
        url: &str,
        form: &[(&str, &str)],
        header: Option<(&str, &str)>,
    ) -> Result<Self::Response, String>;

    fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<Self::Response, String>;
}

/// Isolates [reqwest::blocking::Response] for testing
#[cfg_attr(test, mockall::automock)]
pub trait Response {
    fn status(&self) -> StatusCode;

    /* 'static is needed by automock */
    fn json<T: DeserializeOwned + 'static>(self) -> Result<T, String>;

    fn bytes(self) -> Result<Bytes, String>;

    fn text(self) -> Result<String, String>;
}

/// Wrapper for [reqwest::blocking::Client]
#[derive(Clone, Debug)]
pub struct ReqwestClient {
    client: reqwest::blocking::Client,
}

impl From<reqwest::blocking::Client> for ReqwestClient {
    fn from(value: reqwest::blocking::Client) -> Self {
        ReqwestClient { client: value }
    }
}

impl Client for ReqwestClient {
    type Response = ReqwestResponse;

    fn post(
        &self,
        url: &str,
        form: &[(&str, &str)],
        header: Option<(&str, &str)>,
    ) -> Result<ReqwestResponse, String> {
        let mut request_builder = self.client.post(url).form(form);
        if let Some((key, value)) = header {
            request_builder = request_builder.header(key, value);
        }
        let response = request_builder.send().map_err_to_string()?;
        Ok(ReqwestResponse { response })
    }

    fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<ReqwestResponse, String> {
        let response = self
            .client
            .get(url)
            .query(query)
            .send()
            .map_err_to_string()?;
        Ok(ReqwestResponse { response })
    }
}

/// Wrapper for [reqwest::blocking::Response]
#[derive(Debug)]
pub struct ReqwestResponse {
    response: reqwest::blocking::Response,
}

impl Response for ReqwestResponse {
    fn status(&self) -> StatusCode {
        self.response.status()
    }

    fn json<T: DeserializeOwned>(self) -> Result<T, String> {
        self.response.json().map_err_to_string()
    }

    fn bytes(self) -> Result<Bytes, String> {
        self.response.bytes().map_err_to_string()
    }

    fn text(self) -> Result<String, String> {
        self.response.text().map_err_to_string()
    }
}
