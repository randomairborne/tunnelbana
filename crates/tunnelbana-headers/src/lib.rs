#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{
    header::{InvalidHeaderName, InvalidHeaderValue},
    HeaderName, HeaderValue, Request, Response,
};
use matchit::Router;
use tower::{Layer, Service};

type BonusHeaders = Arc<[(HeaderName, HeaderValue)]>;

#[macro_use]
extern crate tracing;

#[derive(Clone, Debug)]
pub struct HeaderGroup {
    pub path: String,
    pub targets: Vec<(HeaderName, HeaderValue)>,
}

/// Parse a list of [`HeaderGroup`]s from a cloudflare-style _headers string.
/// # Errors
/// This function errors if you have an orphaned header definition, if you have an invalid header name or value,
/// or if your name cannot be a matchit path.
pub fn parse(header_file: &str) -> Result<Vec<HeaderGroup>, HeaderParseError> {
    let mut headers = Vec::new();
    let mut current_ctx: Option<HeaderGroup> = None;
    for (idx, line) in header_file.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            // handle comments
            continue;
        }
        if line.starts_with(['\t', ' ']) {
            let Some(ctx) = current_ctx.as_mut() else {
                return Err(HeaderParseError::new(HeaderParseErrorKind::NoParseCtx, idx));
            };
            let (name, value) = line
                .trim()
                .split_once(':')
                .ok_or_else(|| HeaderParseError::new(HeaderParseErrorKind::NoHeaderColon, idx))?;
            let name = match HeaderName::from_bytes(name.trim().as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    return Err(HeaderParseError::new(
                        HeaderParseErrorKind::HeaderNameParse(e),
                        idx,
                    ))
                }
            };
            let value = match HeaderValue::from_bytes(value.trim().as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    return Err(HeaderParseError::new(
                        HeaderParseErrorKind::HeaderValueParse(e),
                        idx,
                    ))
                }
            };

            ctx.targets.push((name, value));
        } else {
            let mut group = Some(HeaderGroup {
                path: line.trim().to_string(),
                targets: Vec::new(),
            });
            std::mem::swap(&mut current_ctx, &mut group);
            if let Some(group) = group {
                headers.push(group);
            }
        }
    }
    if let Some(ctx) = current_ctx {
        headers.push(ctx);
    }
    info!(?headers, "Got headers");
    Ok(headers)
}

#[derive(Debug, thiserror::Error)]
#[error("at line {row}: {kind}")]
pub struct HeaderParseError {
    row: usize,
    #[source]
    kind: HeaderParseErrorKind,
}

impl HeaderParseError {
    const fn new(kind: HeaderParseErrorKind, idx: usize) -> Self {
        Self { row: idx + 1, kind }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HeaderParseErrorKind {
    #[error("Header name invalid: {0}")]
    HeaderNameParse(#[from] InvalidHeaderName),
    #[error("Header name value: {0}")]
    HeaderValueParse(#[from] InvalidHeaderValue),
    #[error("You must specify an unindented path before specifying headers")]
    NoParseCtx,
    #[error("You must put a colon at the end of the header name")]
    NoHeaderColon,
}

#[derive(Clone)]
pub struct HeadersLayer {
    headers: Arc<matchit::Router<BonusHeaders>>,
}

impl HeadersLayer {
    /// Create a new [`HeadersLayer`]. The header groups are naively added
    /// to a matchit router internally.
    /// # Errors
    /// If two [`HeaderGroup`]s are the same, or would illgally overlap
    /// an error can be returned
    pub fn new(header_list: Vec<HeaderGroup>) -> Result<Self, Error> {
        let mut headers = Router::new();
        for header in header_list {
            headers.insert(header.path, header.targets.into())?;
        }

        info!(?headers, "Built auto header map");

        Ok(Self {
            headers: Arc::new(headers),
        })
    }
}

impl<S> Layer<S> for HeadersLayer {
    type Service = Headers<S>;

    fn layer(&self, inner: S) -> Headers<S> {
        Headers {
            headers: self.headers.clone(),
            inner,
        }
    }
}

#[derive(Clone)]
pub struct Headers<S> {
    headers: Arc<matchit::Router<BonusHeaders>>,
    inner: S,
}

#[pin_project::pin_project]
pub struct ResponseFuture<F> {
    #[pin]
    src: F,
    additional_headers: Option<BonusHeaders>,
}

impl<F, B, BE> std::future::Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<B>, Infallible>>,
    B: http_body::Body<Data = Bytes, Error = BE> + Send + 'static,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let bonus_headers = self.additional_headers.clone();
        self.project()
            .src
            .poll(cx)
            .map(|v| add_headers(v, bonus_headers))
    }
}

#[allow(clippy::unnecessary_wraps)]
fn add_headers<B>(
    res: Result<Response<B>, Infallible>,
    bonus_headers: Option<BonusHeaders>,
) -> Result<Response<B>, Infallible> {
    let Ok(mut inner) = res;
    let resp_headers = inner.headers_mut();
    if let Some(bonus_headers) = bonus_headers {
        for (name, value) in bonus_headers.iter() {
            resp_headers.insert(name.clone(), value.clone());
        }
    }
    Ok(inner)
}

impl<ReqBody, F, FResBody, FResBodyError> Service<Request<ReqBody>> for Headers<F>
where
    F: Service<Request<ReqBody>, Response = Response<FResBody>, Error = Infallible> + Clone,
    F::Future: Send + 'static,
    FResBody: http_body::Body<Data = Bytes, Error = FResBodyError> + Send + 'static,
    FResBodyError: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Error = Infallible;
    type Future = ResponseFuture<F::Future>;
    type Response = Response<FResBody>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        let path = req.uri().path();
        let additional_headers = self.headers.at(path).ok().map(|v| v.value.clone());
        ResponseFuture {
            src: self.inner.call(req),
            additional_headers,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Could not add route: {0}")]
    Insert(#[from] matchit::InsertError),
}
