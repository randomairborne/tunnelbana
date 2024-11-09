use std::{
    collections::HashMap,
    convert::Infallible,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{header::InvalidHeaderValue, HeaderValue, Request, Response, StatusCode};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt};
use tower::{Layer, Service};

#[macro_use]
extern crate tracing;

#[derive(Debug)]
pub struct ETagMap {
    map: HashMap<String, HeaderValue>,
}

impl ETagMap {
    pub fn new(base_dir: &Path) -> Result<Self, Error> {
        let files = get_file_list(base_dir)?;
        trace!(?files, count = files.len(), "Hashing files");
        let mut map = HashMap::new();
        for path in files {
            // This is basically just `b3sum` but rust
            trace!(?path, "Hashing file");
            let hash = blake3::Hasher::new().update_mmap_rayon(&path)?.finalize();
            let relative_path = path
                .strip_prefix(base_dir)?
                .to_str()
                .ok_or(Error::PathNotStr)?;
            let key = format!("/{relative_path}");
            let hash = hash.to_hex();
            let value = HeaderValue::from_str(&format!("\"{hash}\""))?;
            trace!(key, ?value, "Hashed file");
            map.insert(key, value);
        }
        info!(count = map.len(), "Hashed files");
        Ok(Self { map })
    }
}

fn get_file_list(path: &Path) -> Result<Vec<PathBuf>, Error> {
    trace!(?path, "Reading directory");
    let dir = std::fs::read_dir(path)?;
    let mut paths = Vec::new();
    for file in dir {
        let file = file?;
        let kind = file.file_type()?;
        let path = file.path();
        if kind.is_dir() {
            let mut dir = get_file_list(&path)?;
            paths.append(&mut dir);
        } else if kind.is_file() {
            trace!(?path, "Found file");
            paths.push(path);
        } else {
            return Err(Error::UnknownFileKind);
        }
    }
    trace!(?paths, "Read directory");
    Ok(paths)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Could not strip prefix: {0}")]
    StripPrefix(#[from] std::path::StripPrefixError),
    #[error("Hex header value was somehow invalid")]
    InvalidHeaderValue(#[from] InvalidHeaderValue),
    #[error("ETagMap does not follow symlinks or other strange files")]
    UnknownFileKind,
    #[error("Path was not a valid UTF-8 string")]
    PathNotStr,
}

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
    ChildRespWithETag(#[pin] F, HeaderValue),
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
            PinResponseOpts::ChildRespWithETag(f, etag) => f
                .poll(cx)
                .map(|v| add_etag(v, etag.clone()))
                .map(unsync_box_body_ify),
            PinResponseOpts::NotModified(etag) => Poll::Ready(Ok(not_modified(etag.clone()))),
        }.map(remove_last_modified)
    }
}

fn add_etag<B>(
    res: Result<Response<B>, Infallible>,
    etag: HeaderValue,
) -> Result<Response<B>, Infallible> {
    let mut inner = res.unwrap(); // This is 100% fine. Infallible is unconstructable.
    inner.headers_mut().insert(http::header::ETAG, etag);
    Ok(inner)
}

fn remove_last_modified<B>(
    res: Result<Response<B>, Infallible>,
) -> Result<Response<B>, Infallible> {
    let mut inner = res.unwrap(); // This is 100% fine. Infallible is unconstructable.
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
        if let Some(tag) = self.tags.map.get(&path) {
            match req.headers().get(http::header::IF_NONE_MATCH) {
                Some(matched) if matched == tag => ResponseFuture::NotModified(tag.clone()),
                _ => ResponseFuture::ChildRespWithETag(self.inner.call(req), tag.clone()),
            }
        } else {
            ResponseFuture::NoETag(self.inner.call(req))
        }
    }
}
