#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{header, HeaderValue, Request, Response, StatusCode};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt};
use matchit::Router;
use tower::{Layer, Service};

#[macro_use]
extern crate tracing;

#[derive(Clone)]
pub struct Redirect {
    pub path: String,
    pub target: HeaderValue,
    pub code: StatusCode,
}

/// Parse a list of [`Redirect`]s from a cloudflare-style _redirects string.
/// # Errors
/// This function errors if your status code is malformed, your target cannot be a header value,
/// or if your name cannot be a matchit path.
pub fn parse(redirect_file: &str) -> Result<Vec<Redirect>, RedirectParseError> {
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
        let Ok(target) = HeaderValue::from_str(items[1].trim()) else {
            return Err(RedirectParseError::new(
                RedirectParseErrorKind::HeaderValue(items[1].to_string()),
                idx,
            ));
        };

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

#[derive(Debug, thiserror::Error)]
#[error("{kind}")]
pub struct RedirectParseError {
    row: usize,
    #[source]
    kind: RedirectParseErrorKind,
}

impl RedirectParseError {
    const fn new(kind: RedirectParseErrorKind, idx: usize) -> Self {
        Self { row: idx + 1, kind }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RedirectParseErrorKind {
    #[error("Wrong number of entries on a line: {0}, expected 2 or 3")]
    WrongOptCount(usize),
    #[error("`{0}` is an invalid header value")]
    HeaderValue(String),
    #[error("`{0}` could not be converted to a status")]
    StatusCode(String),
}

#[derive(Clone)]
pub struct RedirectsLayer {
    redirects: Arc<matchit::Router<(HeaderValue, StatusCode)>>,
}

impl RedirectsLayer {
    /// Create a new [`RedirectsLayer`] from a list of [`Redirect`]s.
    /// # Errors
    /// This function can error if you have two redirects for the same path.
    pub fn new(redirect_list: Vec<Redirect>) -> Result<Self, Error> {
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
pub struct Redirects<S> {
    redirects: Arc<matchit::Router<(HeaderValue, StatusCode)>>,
    inner: S,
}

#[pin_project::pin_project(project = PinResponseSource)]
pub enum ResponseFuture<F> {
    Child(#[pin] F),
    Redirect(HeaderValue, StatusCode),
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
                Poll::Ready(Ok(redirect_respond(header_value.clone(), *status)))
            }
            PinResponseSource::Child(f) => f.poll(cx).map(unsync_box_body_ify),
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
    value: HeaderValue,
    code: StatusCode,
) -> http::Response<UnsyncBoxBody<Bytes, E>> {
    let mut response = Response::new(UnsyncBoxBody::new(
        http_body_util::Empty::new().map_err(|never| match never {}),
    ));
    response.headers_mut().insert(header::LOCATION, value);
    *response.status_mut() = code;
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
            ResponseFuture::Redirect(location.value.0.clone(), location.value.1)
        } else {
            ResponseFuture::Child(self.inner.call(req))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Could not add route: {0}")]
    Insert(#[from] matchit::InsertError),
}
