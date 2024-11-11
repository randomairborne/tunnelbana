#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
//! # tunnelbana-hidepaths
//! Hide specific paths in tower services by sending them to a 404 service.
//!
//! Part of the [tunnelbana](https://github.com/randomairborne/tunnelbana) project.
//!
//! # Example
//! ```rust
//! use tower_http::services::ServeDir;
//! use tower::{ServiceBuilder, ServiceExt};
//! use http::Response;
//! use tunnelbana_hidepaths::HidePathsLayer;
//!
//! let hidepaths_middleware = HidePathsLayer::builder()
//!     .hide("/_redirects")
//!     .hide_all(["/.htaccess", "/.well-known/{*hide}"])
//!     .build()
//!     .expect("Failed to build path hide router");
//! let serve_dir = ServeDir::new("/var/www/html").append_index_html_on_directories(true);
//! let service = ServiceBuilder::new()
//!    .layer(hidepaths_middleware)
//!    .service(serve_dir);
//! ```
use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::Either;
pub use matchit::InsertError;
use tower::{Layer, Service};

#[derive(Clone)]
/// Build a [`matchit::Router`] of paths which should be routed to
/// the not found service.
///
/// The not found service defaults to [`DefaultNotFoundService`],
/// however it is very barebones, so it is reccomended to supply your own with [`Self::with_not_found_service`].
pub struct HidePathsLayerBuilder<N = DefaultNotFoundService> {
    hidden: matchit::Router<()>,
    notfound: N,
    errors: Vec<(String, InsertError)>,
}

impl<N> HidePathsLayerBuilder<N> {
    #[must_use]
    /// Create a new builder with the [`DefaultNotFoundService`].
    pub fn new() -> HidePathsLayerBuilder<DefaultNotFoundService> {
        HidePathsLayerBuilder {
            hidden: matchit::Router::new(),
            notfound: DefaultNotFoundService,
            errors: Vec::new(),
        }
    }

    /// Use a different service for 404'd files than the [`DefaultNotFoundService`].
    pub fn with_not_found_service<T>(self, notfound: T) -> HidePathsLayerBuilder<T> {
        HidePathsLayerBuilder {
            notfound,
            hidden: self.hidden,
            errors: self.errors,
        }
    }

    #[must_use]
    /// All [`matchit`] routes passed to this method will be routed to the not found service.
    pub fn hide(mut self, route: impl Into<String>) -> Self {
        let route = route.into();
        if let Err(err) = self.hidden.insert(&route, ()) {
            self.errors.push((route, err));
        }
        self
    }

    #[must_use]
    /// Convenience method for calling [`Self::hide`] in a loop.
    pub fn hide_all<IS: Into<String>>(mut self, routes: impl IntoIterator<Item = IS>) -> Self {
        for route in routes {
            self = self.hide(route);
        }
        self
    }

    /// Get a list of errors which have occured inside the builder.
    pub fn errors(&self) -> &[(String, InsertError)] {
        self.errors.as_slice()
    }

    /// Build this [`HidePathsLayer`].
    /// # Errors
    /// This function errors if matchit has had any errors while inserting-
    /// you get the path that was inserted, and the error.
    pub fn build(self) -> Result<HidePathsLayer<N>, Vec<(String, InsertError)>> {
        if !self.errors.is_empty() {
            return Err(self.errors);
        }
        Ok(HidePathsLayer {
            hidden: Arc::new(self.hidden),
            notfound: self.notfound,
        })
    }
}

#[derive(Clone)]
/// A [`tower::Layer`] for use with a [`tower::ServiceBuilder`] to reply with a fallback
/// service to any routes found internally.
pub struct HidePathsLayer<N = DefaultNotFoundService> {
    hidden: Arc<matchit::Router<()>>,
    notfound: N,
}

impl HidePathsLayer<DefaultNotFoundService> {
    #[must_use]
    pub fn builder() -> HidePathsLayerBuilder<DefaultNotFoundService> {
        HidePathsLayerBuilder::<DefaultNotFoundService>::new()
    }
}

impl<S, N> Layer<S> for HidePathsLayer<N>
where
    N: Clone,
{
    type Service = HidePath<S, N>;

    fn layer(&self, inner: S) -> HidePath<S, N> {
        HidePath {
            hidden: self.hidden.clone(),
            notfound: self.notfound.clone(),
            inner,
        }
    }
}

#[derive(Clone)]
/// A wrapper service which forwards to one of two inner services based on if the requested
/// path is contained within its internal router.
pub struct HidePath<S, N> {
    hidden: Arc<matchit::Router<()>>,
    notfound: N,
    inner: S,
}

#[pin_project::pin_project(project = PinResponseSource)]
/// Future which always delegates the whole response to either the default service, or
/// a not-found fallback, and returns the service response unmodified.
pub enum ResponseFuture<S, N> {
    Child(#[pin] S),
    NotFound(#[pin] N),
}

impl<S, N, SB, NB, SBE, NBE> std::future::Future for ResponseFuture<S, N>
where
    S: Future<Output = Result<Response<SB>, Infallible>>,
    N: Future<Output = Result<Response<NB>, Infallible>>,
    SB: http_body::Body<Data = Bytes, Error = SBE> + Send + 'static,
    NB: http_body::Body<Data = Bytes, Error = NBE> + Send + 'static,
{
    type Output = Result<Response<Either<SB, NB>>, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            PinResponseSource::Child(s) => s.poll(cx).map(|v| {
                v.map(|resp| {
                    let (parts, body) = resp.into_parts();
                    Response::from_parts(parts, Either::Left(body))
                })
            }),
            PinResponseSource::NotFound(s) => s.poll(cx).map(|v| {
                v.map(|resp| {
                    let (parts, body) = resp.into_parts();
                    Response::from_parts(parts, Either::Right(body))
                })
            }),
        }
    }
}

impl<ReqBody, S, SResBody, SResBodyError, N, NResBody, NResBodyError> Service<Request<ReqBody>>
    for HidePath<S, N>
where
    S: Service<Request<ReqBody>, Response = Response<SResBody>, Error = Infallible> + Clone,
    S::Future: Send + 'static,
    SResBody: http_body::Body<Data = Bytes, Error = SResBodyError> + Send + 'static,
    SResBodyError: Into<Box<dyn std::error::Error + Send + Sync>>,
    N: Service<Request<ReqBody>, Response = Response<NResBody>, Error = Infallible> + Clone,
    N::Future: Send + 'static,
    NResBody: http_body::Body<Data = Bytes, Error = NResBodyError> + Send + 'static,
    NResBodyError: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Error = Infallible;
    type Future = ResponseFuture<S::Future, N::Future>;
    type Response = Response<http_body_util::Either<SResBody, NResBody>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        let path = req.uri().path();
        if self.hidden.at(path).is_ok() {
            tracing::info!(?path, "Blocked request");
            ResponseFuture::NotFound(self.notfound.call(req))
        } else {
            ResponseFuture::Child(self.inner.call(req))
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
/// Unconfigurable service which returns HTTP 404s with no body.
pub struct DefaultNotFoundService;

/// Future type which returns an empty HTTP 404.
pub struct DefaultNotFoundFuture;

impl<T> Service<T> for DefaultNotFoundService {
    type Error = Infallible;
    type Future = DefaultNotFoundFuture;
    type Response = Response<http_body_util::Empty<Bytes>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: T) -> Self::Future {
        DefaultNotFoundFuture
    }
}

impl std::future::Future for DefaultNotFoundFuture {
    type Output = Result<Response<http_body_util::Empty<Bytes>>, Infallible>;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let mut resp = Response::new(http_body_util::Empty::new());
        *resp.status_mut() = StatusCode::NOT_FOUND;
        Poll::Ready(Ok(resp))
    }
}
