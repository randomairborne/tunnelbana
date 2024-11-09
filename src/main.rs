#![allow(unused)]

use std::{
    io::{Error as IoError, ErrorKind as IoErrorKind},
    path::Path,
    pin::pin,
    time::Duration,
};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Empty};
use hyper_util::{
    rt::TokioExecutor,
    server::{conn::auto::Builder as ConnBuilder, graceful::GracefulShutdown},
    service::TowerToHyperService,
};
use tokio::net::TcpListener;
use tower::{ServiceBuilder, ServiceExt};
use tower_http::{
    services::{ServeDir, ServeFile},
    set_status::{SetStatus, SetStatusLayer},
    validate_request::ValidateRequestHeaderLayer,
};
use tracing::Level;
use tunnelbana_headers::HeadersLayer;
use tunnelbana_redirects::RedirectsLayer;

#[macro_use]
extern crate tracing;

const RESERVED_PATHS: [&str; 2] = ["/_headers", "/_redirects"];

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .init();
    let arg1 = std::env::args_os()
        .nth(1)
        .display_expect("Expected exactly 1 argument");
    let location = Path::new(&arg1);
    if !location.is_dir() {
        panic!("Expected argument 1 to be a directory");
    }
    let location = location.canonicalize().unwrap();

    let headers = read_with_default_if_nonexistent(location.join("_headers"))
        .display_expect("Failed to read _headers");
    let headers = tunnelbana_headers::parse(&headers).display_expect("Failed to parse _headers");

    let redirects = read_with_default_if_nonexistent(location.join("_redirects"))
        .display_expect("Failed to read _redirects");
    let redirects =
        tunnelbana_redirects::parse(&redirects).display_expect("Failed to parse _redirects");

    let redirect_mw =
        RedirectsLayer::new(redirects).display_expect("Failed to build redirects router");
    let header_add_mw = HeadersLayer::new(headers).display_expect("Failed to build headers router");

    let not_found_svc = ServeFile::new(location.join("404.html"))
        .precompressed_br()
        .precompressed_deflate()
        .precompressed_gzip()
        .precompressed_zstd();
    let not_found_svc = ServiceBuilder::new()
        .layer(SetStatusLayer::new(StatusCode::NOT_FOUND))
        .service(not_found_svc);
    let serve_dir = ServeDir::new(location)
        .append_index_html_on_directories(true)
        .precompressed_br()
        .precompressed_deflate()
        .precompressed_gzip()
        .precompressed_zstd()
        .fallback(not_found_svc);

    let hide_special_files = ValidateRequestHeaderLayer::custom(
        |req: &mut Request<_>| -> Result<(), Response<UnsyncBoxBody<Bytes, IoError>>> {
            for reserved_start in RESERVED_PATHS {
                let path = req.uri().path();
                if path.starts_with(reserved_start)
                    && !path.trim_start_matches(reserved_start).contains('/')
                {
                    return {
                        let mut resp = Response::new(UnsyncBoxBody::new(
                            Empty::new().map_err(|never| match never {}),
                        ));
                        *resp.status_mut() = StatusCode::NOT_FOUND;
                        Err(resp)
                    };
                }
            }
            Ok(())
        },
    );

    let service = ServiceBuilder::new()
        .layer(header_add_mw)
        .layer(redirect_mw)
        .layer(hide_special_files)
        .map_response(|res: Response<_>| res.map(UnsyncBoxBody::new))
        .service(serve_dir);

    let listener = TcpListener::bind("0.0.0.0:8080")
        .await
        .display_expect("Failed to bind to port 8080");

    let server = ConnBuilder::new(TokioExecutor::new());
    let graceful = GracefulShutdown::new();
    let mut ctrl_c = pin!(vss::shutdown_signal());

    loop {
        let service = service.clone();
        tokio::select! {
            conn = listener.accept() => {
                match conn {
                    Ok((stream, peer_addr)) => {
                        info!("incoming connection accepted: {}", peer_addr);
                        let stream = hyper_util::rt::TokioIo::new(Box::pin(stream));

                        let conn = server.serve_connection_with_upgrades(stream, TowerToHyperService::new(service)).into_owned();
                        let conn = graceful.watch(conn.into_owned());

                        tokio::spawn(async move {
                            if let Err(err) = conn.await {
                                warn!("connection error: {}", err);
                            }
                            debug!("connection dropped: {}", peer_addr);
                        });
                    },
                    Err(e) => {
                        warn!("accept error: {}", e);
                        return;
                    }
                };
            },
            _ = ctrl_c.as_mut() => {
                drop(listener);
                info!("Ctrl-C received, starting shutdown");
                break;
            }
        }
    }

    tokio::select! {
        _ = graceful.shutdown() => {
            info!("Gracefully shutdown!");
        },
        _ = tokio::time::sleep(Duration::from_secs(10)) => {
            error!("Waited 10 seconds for graceful shutdown, aborting...");
        }
    }
}

fn read_with_default_if_nonexistent(path: impl AsRef<Path>) -> Result<String, IoError> {
    match std::fs::read_to_string(path.as_ref()) {
        Ok(v) => Ok(v),
        Err(e) => match e.kind() {
            IoErrorKind::NotFound => Ok(String::new()),
            _ => Err(e),
        },
    }
}

pub trait DisplayExpect<T> {
    fn display_expect(self, message: &str) -> T;
}

impl<T, E: std::fmt::Display> DisplayExpect<T> for Result<T, E> {
    fn display_expect(self, msg: &str) -> T {
        match self {
            Ok(v) => v,
            Err(e) => panic!("{msg}: {e}"),
        }
    }
}

impl<T> DisplayExpect<T> for Option<T> {
    fn display_expect(self, msg: &str) -> T {
        if let Some(v) = self {
            v
        } else {
            panic!("{msg}");
        }
    }
}
