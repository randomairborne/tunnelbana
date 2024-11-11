#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
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
pub struct HidePathLayerBuilder<N = DefaultNotFoundService> {
    hidden: matchit::Router<()>,
    fallback_svc: N,
    errors: Vec<(String, InsertError)>,
}

impl<N> HidePathLayerBuilder<N> {
    #[must_use]
    pub fn new() -> HidePathLayerBuilder<DefaultNotFoundService> {
        HidePathLayerBuilder {
            hidden: matchit::Router::new(),
            fallback_svc: DefaultNotFoundService,
            errors: Vec::new(),
        }
    }

    pub fn with_fallback<T>(self, fallback_svc: T) -> HidePathLayerBuilder<T> {
        HidePathLayerBuilder {
            fallback_svc,
            hidden: self.hidden,
            errors: self.errors,
        }
    }

    #[must_use]
    pub fn hide(mut self, route: impl Into<String>) -> Self {
        let route = route.into();
        if let Err(err) = self.hidden.insert(&route, ()) {
            self.errors.push((route, err));
        }
        self
    }

    #[must_use]
    pub fn hide_all<IS: Into<String>>(mut self, routes: impl IntoIterator<Item = IS>) -> Self {
        for route in routes {
            self = self.hide(route);
        }
        self
    }

    pub fn errors(&self) -> &[(String, InsertError)] {
        self.errors.as_slice()
    }

    /// Build this [`HidePathLayer`].
    /// # Errors
    /// This function errors if matchit has had any errors while inserting-
    /// you get the path that was inserted, and the error.
    pub fn build(self) -> Result<HidePathLayer<N>, Vec<(String, InsertError)>> {
        if !self.errors.is_empty() {
            return Err(self.errors);
        }
        Ok(HidePathLayer {
            hidden: Arc::new(self.hidden),
            fallback_svc: self.fallback_svc,
        })
    }
}

#[derive(Clone)]
pub struct HidePathLayer<N = DefaultNotFoundService> {
    hidden: Arc<matchit::Router<()>>,
    fallback_svc: N,
}

impl HidePathLayer<DefaultNotFoundService> {
    #[must_use]
    pub fn builder() -> HidePathLayerBuilder<DefaultNotFoundService> {
        HidePathLayerBuilder::<DefaultNotFoundService>::new()
    }
}

impl<S, N> Layer<S> for HidePathLayer<N>
where
    N: Clone,
{
    type Service = HidePath<S, N>;

    fn layer(&self, inner: S) -> HidePath<S, N> {
        HidePath {
            hidden: self.hidden.clone(),
            notfound: self.fallback_svc.clone(),
            inner,
        }
    }
}

#[derive(Clone)]
pub struct HidePath<S, N> {
    hidden: Arc<matchit::Router<()>>,
    notfound: N,
    inner: S,
}

#[pin_project::pin_project(project = PinResponseSource)]
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
pub struct DefaultNotFoundService;

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
