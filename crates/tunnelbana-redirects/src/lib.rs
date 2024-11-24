#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
//! # tunnelbana-redirects
//! Generate redirect lists from cloudflare-style _redirects text files and serve them with tower.
//!
//! Part of the [tunnelbana](https://github.com/randomairborne/tunnelbana) project.
//!
//! # Example
//! ```rust
//! use tower_http::services::ServeDir;
//! use tower::{ServiceBuilder, ServiceExt};
//! use http::Response;
//! use tunnelbana_redirects::RedirectsLayer;
//!
//! let config = r#"
//!/example https://example.com 302
//!/subpath/{other}/final /{other}/final/ 302
//!/wildcard/{*wildcard} /{wildcard}
//!"#;
//! let redirects = tunnelbana_redirects::parse(config).expect("Failed to parse redirects");
//! let redirects_mw = RedirectsLayer::new(redirects).expect("Failed to route redirects");
//! let serve_dir = ServeDir::new("/var/www/html").append_index_html_on_directories(true);
//! let service = ServiceBuilder::new()
//!    .layer(redirects_mw)
//!    .service(serve_dir);
//! ```
use std::{
    borrow::Cow,
    collections::HashMap,
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{header, HeaderValue, Request, Response, StatusCode};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt};
pub use matchit::InsertError;
use matchit::Router;
use simpleinterpolation::{Interpolation, RenderError};
use tower::{Layer, Service};

#[macro_use]
extern crate tracing;

#[derive(Clone)]
/// A representation of a redirect, with where it should go and its triggers.
pub struct Redirect {
    pub path: String,
    pub target: Interpolation,
    pub code: StatusCode,
}

/// Parse a list of [`Redirect`]s from a cloudflare-style _redirects string.
/// # Errors
/// This function errors if your status code is malformed, your target cannot be a header value,
/// or if your name cannot be a matchit path.
pub fn parse(redirect_file: &str) -> Result<Vec<Redirect>, RedirectParseError> {
    if redirect_file.is_empty() {
        return Ok(Vec::new());
    }
    let mut redirects = Vec::new();
    for (idx, line) in redirect_file.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            // handle comments
            continue;
        }

        let items = line.split(' ').collect::<Vec<&str>>();
        info!(line = idx + 1, ?items, "Items for line");
        if !(2..=3).contains(&items.len()) {
            return Err(RedirectParseError::new(
                RedirectParseErrorKind::WrongOptCount(items.len()),
                idx,
            ));
        }

        let path = items[0].to_string();
        let target = Interpolation::new(items[1])
            .map_err(|e| RedirectParseError::new(RedirectParseErrorKind::Interpolation(e), idx))?;

        test_interpolation(&path, &target, idx)?;

        let code: StatusCode = if let Some(code_str) = items.get(2) {
            let Ok(code) = code_str.parse() else {
                return Err(RedirectParseError::new(
                    RedirectParseErrorKind::StatusCode((*code_str).to_string()),
                    idx,
                ));
            };
            code
        } else {
            StatusCode::TEMPORARY_REDIRECT
        };
        redirects.push(Redirect { path, target, code });
    }
    Ok(redirects)
}

fn test_interpolation(
    path: &str,
    target: &Interpolation,
    idx: usize,
) -> Result<(), RedirectParseError> {
    // Show a valid matchit route
    let mut router = matchit::Router::new();
    router
        .insert(path, ())
        .map_err(|e| RedirectParseError::new(RedirectParseErrorKind::Matchit(e), idx))?;

    // params returns (key, value)
    let params: HashMap<Cow<str>, Cow<str>> = router
        .at(path)
        .map_err(|_| {
            RedirectParseError::new(RedirectParseErrorKind::NonSelfMatchingTriggerPath, idx)
        })?
        .params
        .iter()
        .map(cowify)
        .collect();

    // prove that this value can actually be rendered
    let render = target.try_render(&params).map_err(|e| {
        let RenderError::UnknownVariables(e) = e;
        RedirectParseError::new(
            RedirectParseErrorKind::InterpKeys(e.into_iter().map(ToOwned::to_owned).collect()),
            idx,
        )
    })?;

    // Prove that the rendered value is a valid header
    HeaderValue::from_bytes(render.as_bytes())
        .map_err(|_| RedirectParseError::new(RedirectParseErrorKind::HeaderValue(render), idx))?;

    Ok(())
}

#[inline]
const fn cowify<'a>(v: (&'a str, &'a str)) -> (Cow<'a, str>, Cow<'a, str>) {
    (Cow::Borrowed(v.0), Cow::Borrowed(v.1))
}

#[derive(Debug, thiserror::Error)]
#[error("at row {row}: {kind}")]
/// Error struct for unparsable redirects. Includes line number and type of error.
pub struct RedirectParseError {
    pub row: usize,
    #[source]
    pub kind: RedirectParseErrorKind,
}

impl RedirectParseError {
    const fn new(kind: RedirectParseErrorKind, idx: usize) -> Self {
        Self { row: idx + 1, kind }
    }
}

#[derive(Debug, thiserror::Error)]
/// Types of errors that can happen, e.g. wrong number of items on a row, unparsable status code.
pub enum RedirectParseErrorKind {
    #[error("Wrong number of entries on a line: {0}, expected 2 or 3")]
    WrongOptCount(usize),
    #[error("`{0}` is an invalid header value")]
    HeaderValue(String),
    #[error("`{0}` could not be converted to a status")]
    StatusCode(String),
    #[error("{0}")]
    Interpolation(simpleinterpolation::ParseError),
    #[error("Not all keys found, missing {0:?}")]
    InterpKeys(Vec<String>),
    #[error("Invalid trigger path: {0}")]
    Matchit(matchit::InsertError),
    #[error("This path doesn't match itself, this is a bug")]
    NonSelfMatchingTriggerPath,
}

#[derive(Clone)]
/// a [`tower::Layer`] to add to a [`tower::ServiceBuilder`] to add redirects.
pub struct RedirectsLayer {
    redirects: Arc<matchit::Router<(Interpolation, StatusCode)>>,
}

impl RedirectsLayer {
    /// Create a new [`RedirectsLayer`] from a list of [`Redirect`]s.
    /// # Errors
    /// This function can error if you have two redirects for the same path.
    pub fn new(redirect_list: Vec<Redirect>) -> Result<Self, InsertError> {
        let mut redirects = Router::new();
        for redirect in redirect_list {
            redirects.insert(redirect.path, (redirect.target, redirect.code))?;
        }

        info!(?redirects, "Built redirect list");

        Ok(Self {
            redirects: Arc::new(redirects),
        })
    }
}

impl<S> Layer<S> for RedirectsLayer {
    type Service = Redirects<S>;

    fn layer(&self, inner: S) -> Redirects<S> {
        Redirects {
            redirects: self.redirects.clone(),
            inner,
        }
    }
}

#[derive(Clone)]
/// a [`tower::Service`] to add redirects to a wrapped service.
pub struct Redirects<S> {
    redirects: Arc<matchit::Router<(Interpolation, StatusCode)>>,
    inner: S,
}

#[pin_project::pin_project(project = PinResponseSource)]
/// Future type which can return an unmodified request, a redirect, or
/// an error if a value in the path capture is not a valid header value.
pub enum ResponseFuture<F> {
    Child(#[pin] F),
    Redirect(HeaderValue, StatusCode),
    InvalidHeaderValue,
}

impl<F, B, BE> std::future::Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<B>, Infallible>>,
    B: http_body::Body<Data = Bytes, Error = BE> + Send + 'static,
{
    type Output = Result<Response<UnsyncBoxBody<Bytes, BE>>, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            PinResponseSource::Redirect(header_value, status) => {
                Poll::Ready(Ok(redirect_respond(header_value, *status)))
            }
            PinResponseSource::Child(f) => f.poll(cx).map(unsync_box_body_ify),
            PinResponseSource::InvalidHeaderValue => Poll::Ready(Ok(invalid_header_respond())),
        }
    }
}

fn unsync_box_body_ify<B, E, BE>(
    res: Result<Response<B>, E>,
) -> Result<Response<UnsyncBoxBody<Bytes, BE>>, E>
where
    B: http_body::Body<Data = Bytes, Error = BE> + Send + 'static,
{
    res.map(|inner| inner.map(UnsyncBoxBody::new))
}

fn redirect_respond<E>(
    value: &HeaderValue,
    code: StatusCode,
) -> http::Response<UnsyncBoxBody<Bytes, E>> {
    let mut response = Response::new(UnsyncBoxBody::new(
        http_body_util::Empty::new().map_err(|never| match never {}),
    ));
    response
        .headers_mut()
        .insert(header::LOCATION, value.clone());
    *response.status_mut() = code;
    response
}

fn invalid_header_respond<E>() -> http::Response<UnsyncBoxBody<Bytes, E>> {
    let mut response = Response::new(UnsyncBoxBody::new(
        http_body_util::Empty::new().map_err(|never| match never {}),
    ));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response
}

impl<ReqBody, F, FResBody, FResBodyError> Service<Request<ReqBody>> for Redirects<F>
where
    F: Service<Request<ReqBody>, Response = Response<FResBody>, Error = Infallible> + Clone,
    F::Future: Send + 'static,
    FResBody: http_body::Body<Data = Bytes, Error = FResBodyError> + Send + 'static,
    FResBodyError: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Error = Infallible;
    type Future = ResponseFuture<F::Future>;
    type Response = Response<UnsyncBoxBody<Bytes, FResBodyError>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        let path = req.uri().path();
        if let Ok(location) = self.redirects.at(path) {
            let args: HashMap<Cow<str>, Cow<str>> = location.params.iter().map(cowify).collect();
            let src = location.value.0.render(&args);
            if let Ok(value) = HeaderValue::from_str(&src) {
                ResponseFuture::Redirect(value, location.value.1)
            } else {
                ResponseFuture::InvalidHeaderValue
            }
        } else {
            ResponseFuture::Child(self.inner.call(req))
        }
    }
}
