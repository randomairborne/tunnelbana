use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use headers::HeaderParseError;
use http::{header, HeaderName, HeaderValue, Request, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use matchit::Router;
use redirects::RedirectParseError;
use tower::{Layer, Service};

type BonusHeaders = Arc<[(HeaderName, HeaderValue)]>;

mod headers;
mod redirects;

#[derive(Clone)]
pub struct TunnelbanaLayer {
    redirects: Arc<matchit::Router<(HeaderValue, StatusCode)>>,
    headers: Arc<matchit::Router<BonusHeaders>>,
}

impl TunnelbanaLayer {
    pub fn new(headers: &str, redirects: &str) -> Result<Self, Error> {
        let header_list = headers::parse(redirects)?;
        let redirect_list = redirects::parse(headers)?;

        let mut redirects = Router::new();
        for redirect in redirect_list {
            redirects.insert(redirect.path, (redirect.target, redirect.code))?;
        }

        let mut headers = Router::new();
        for header in header_list {
            headers.insert(header.path, header.targets.into())?;
        }

        Ok(Self {
            redirects: Arc::new(redirects),
            headers: Arc::new(headers),
        })
    }
}

impl<S> Layer<S> for TunnelbanaLayer {
    type Service = Tunnelbana<S>;

    fn layer(&self, inner: S) -> Tunnelbana<S> {
        Tunnelbana {
            redirects: self.redirects.clone(),
            headers: self.headers.clone(),
            inner,
        }
    }
}

#[derive(Clone)]
pub struct Tunnelbana<S> {
    redirects: Arc<matchit::Router<(HeaderValue, StatusCode)>>,
    headers: Arc<matchit::Router<BonusHeaders>>,
    inner: S,
}

#[pin_project::pin_project]
pub struct ResponseFuture<F> {
    #[pin]
    src: ResponseSource<F>,
    additional_headers: Option<BonusHeaders>,
}

#[pin_project::pin_project(project = PinResponseSource)]
pub enum ResponseSource<F> {
    Child(#[pin] F),
    Redirect(HeaderValue, StatusCode),
}

impl<F, B> std::future::Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<B>, Infallible>>,
    B: http_body::Body<Data = Bytes, Error = Infallible> + Send + 'static,
{
    type Output = Result<Response<UnsyncBoxBody<Bytes, Infallible>>, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let bonus_headers = self.additional_headers.clone();
        match self.project().src.project() {
            PinResponseSource::Redirect(header_value, status) => {
                Poll::Ready(Ok(redirect_respond(header_value.clone(), *status)))
            }
            PinResponseSource::Child(f) => f.poll(cx).map(unsync_box_body_ify),
        }
        .map(|v| add_headers(v, bonus_headers))
    }
}

fn unsync_box_body_ify<B>(
    res: Result<Response<B>, Infallible>,
) -> Result<Response<UnsyncBoxBody<Bytes, Infallible>>, Infallible>
where
    B: http_body::Body<Data = Bytes, Error = Infallible> + Send + 'static,
{
    let inner = res.unwrap(); // This is 100% fine. Infallible is unconstructable.
    let (parts, body) = inner.into_parts();
    Ok(Response::from_parts(parts, UnsyncBoxBody::new(body)))
}

fn add_headers<B>(
    res: Result<Response<B>, Infallible>,
    bonus_headers: Option<BonusHeaders>,
) -> Result<Response<B>, Infallible> {
    let mut inner = res.unwrap(); // This is 100% fine. Infallible is unconstructable.
    let resp_headers = inner.headers_mut();
    if let Some(bonus_headers) = bonus_headers {
        for (name, value) in bonus_headers.iter() {
            resp_headers.insert(name.clone(), value.clone());
        }
    }
    Ok(inner)
}

fn redirect_respond(
    value: HeaderValue,
    code: StatusCode,
) -> http::Response<UnsyncBoxBody<Bytes, Infallible>> {
    let mut response = Response::new(UnsyncBoxBody::new(http_body_util::Empty::new()));
    response.headers_mut().insert(header::LOCATION, value);
    *response.status_mut() = code;
    response
}

impl<ReqBody, F, FResBody> Service<Request<ReqBody>> for Tunnelbana<F>
where
    F: Service<Request<ReqBody>, Response = Response<FResBody>, Error = Infallible> + Clone,
    F::Future: Send + 'static,
    FResBody: http_body::Body<Data = Bytes, Error = Infallible> + Send + 'static,
    FResBody::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Error = Infallible;
    type Future = ResponseFuture<F::Future>;
    type Response = Response<UnsyncBoxBody<Bytes, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        let path = req.uri().path();
        let additional_headers = self.headers.at(path).ok().map(|v| v.value.clone());
        if let Ok(location) = self.redirects.at(path) {
            ResponseFuture {
                src: ResponseSource::Redirect(location.value.0.clone(), location.value.1),
                additional_headers,
            }
        } else {
            ResponseFuture {
                src: ResponseSource::Child(self.inner.call(req)),
                additional_headers,
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Header parse error: {0}")]
    HeaderParse(#[from] HeaderParseError),
    #[error("Redirect parse error: {0}")]
    RedirectParse(#[from] RedirectParseError),
    #[error("Could not add route: {0}")]
    Insert(#[from] matchit::InsertError),
}
