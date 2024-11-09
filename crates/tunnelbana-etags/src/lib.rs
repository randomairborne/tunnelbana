use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt};
use tag_map::ResourceTagSet;
use tower::{Layer, Service};

#[macro_use]
extern crate tracing;

mod tag_map;
pub use tag_map::{ETagMap, TagMapBuildError};

#[derive(Clone)]
pub struct ETagLayer {
    tags: Arc<ETagMap>,
}

impl ETagLayer {
    pub fn new(tags: ETagMap) -> Self {
        Self {
            tags: Arc::new(tags),
        }
    }
}

impl<S> Layer<S> for ETagLayer {
    type Service = ETag<S>;

    fn layer(&self, inner: S) -> ETag<S> {
        ETag {
            tags: self.tags.clone(),
            inner,
        }
    }
}

#[derive(Clone)]
pub struct ETag<S> {
    tags: Arc<ETagMap>,
    inner: S,
}

#[pin_project::pin_project(project = PinResponseOpts)]
pub enum ResponseFuture<F> {
    NoETag(#[pin] F),
    ChildRespWithETag(#[pin] F, Arc<ResourceTagSet>),
    NotModified(HeaderValue),
}

impl<F, B, BE> std::future::Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<B>, Infallible>>,
    B: http_body::Body<Data = Bytes, Error = BE> + Send + 'static,
{
    type Output = Result<Response<UnsyncBoxBody<Bytes, BE>>, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            PinResponseOpts::NoETag(f) => f.poll(cx).map(unsync_box_body_ify),
            PinResponseOpts::ChildRespWithETag(f, rtags) => f
                .poll(cx)
                .map(|v| add_etag(v, rtags.clone()))
                .map(unsync_box_body_ify),
            PinResponseOpts::NotModified(etag) => Poll::Ready(Ok(not_modified(etag.clone()))),
        }
        .map(remove_last_modified)
    }
}

fn add_etag<B>(
    res: Result<Response<B>, Infallible>,
    etag: Arc<ResourceTagSet>,
) -> Result<Response<B>, Infallible> {
    let Ok(mut inner) = res;
    let etag = if let Some(encoding) = inner.headers().get(http::header::CONTENT_ENCODING) {
        let etag = match encoding.as_bytes() {
            b"gzip" => etag.gzip.clone(),
            b"deflate" => etag.deflate.clone(),
            b"br" => etag.brotli.clone(),
            b"zstd" => etag.zstd.clone(),
            _ => return Ok(inner),
        };
        let Some(etag) = etag else {
            return Ok(inner);
        };
        etag
    } else {
        etag.raw.clone()
    };
    inner.headers_mut().insert(http::header::ETAG, etag);
    Ok(inner)
}

fn remove_last_modified<B>(
    res: Result<Response<B>, Infallible>,
) -> Result<Response<B>, Infallible> {
    let Ok(mut inner) = res;
    inner.headers_mut().remove(http::header::LAST_MODIFIED);
    Ok(inner)
}

fn not_modified<E>(etag: HeaderValue) -> http::Response<UnsyncBoxBody<Bytes, E>> {
    let mut response = Response::new(UnsyncBoxBody::new(
        http_body_util::Empty::new().map_err(|e| match e {}),
    ));
    response.headers_mut().insert(http::header::ETAG, etag);
    *response.status_mut() = StatusCode::NOT_MODIFIED;
    response
}

fn unsync_box_body_ify<B, E, BE>(
    res: Result<Response<B>, E>,
) -> Result<Response<UnsyncBoxBody<Bytes, BE>>, E>
where
    B: http_body::Body<Data = Bytes, Error = BE> + Send + 'static,
{
    res.map(|inner| inner.map(UnsyncBoxBody::new))
}

impl<ReqBody, F, FResBody, FResBodyError> Service<Request<ReqBody>> for ETag<F>
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
        let path = if path.ends_with('/') {
            format!("{path}index.html")
        } else {
            path.to_string()
        };
        if let Some(tags) = self.tags.get(&path) {
            match req.headers().get(http::header::IF_NONE_MATCH) {
                Some(matched) if tags.contains_tag(matched) => {
                    ResponseFuture::NotModified(matched.clone())
                }
                _ => ResponseFuture::ChildRespWithETag(self.inner.call(req), tags.clone()),
            }
        } else {
            ResponseFuture::NoETag(self.inner.call(req))
        }
    }
}
