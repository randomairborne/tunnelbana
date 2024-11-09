use std::{
    io::{Error as IoError, ErrorKind as IoErrorKind},
    path::Path,
    pin::pin,
    time::Duration,
};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{combinators::BoxBody, Empty};
use hyper_util::{
    rt::TokioExecutor,
    server::{conn::auto::Builder as ConnBuilder, graceful::GracefulShutdown},
    service::TowerToHyperService,
};
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::{services::ServeDir, validate_request::ValidateRequestHeaderLayer};
use tunnelbana_tower::TunnelbanaLayer;

#[macro_use]
extern crate tracing;

const RESERVED_PATHS: [&str; 2] = ["/_headers", "/_redirects"];

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let arg1 = std::env::args_os()
        .nth(1)
        .expect("Expected exactly 1 argument");
    let location = Path::new(&arg1);
    if !location.is_dir() {
        panic!("Expected argument 1 to be a directory");
    }
    let headers = read_with_default_if_nonexistent(location.with_file_name("_headers"))
        .expect("Failed to read _headers");
    let redirects = read_with_default_if_nonexistent(location.with_file_name("_redirects"))
        .expect("Failed to read _redirects");

    let tunnelbanna =
        TunnelbanaLayer::new(&headers, &redirects).expect("Failed to parse _headers or _redirects");
    let serve_dir = ServeDir::new(location);

    let hide_special_files = ValidateRequestHeaderLayer::custom(
        |req: &mut Request<_>| -> Result<(), Response<BoxBody<Bytes, _>>> {
            for reserved_start in RESERVED_PATHS {
                let path = req.uri().path();
                if path.starts_with(reserved_start)
                    && !path.trim_start_matches(reserved_start).contains('/')
                {
                    return {
                        let mut resp = Response::new(BoxBody::new(Empty::new()));
                        *resp.status_mut() = StatusCode::NOT_FOUND;
                        Err(resp)
                    };
                }
            }
            Ok(())
        },
    );

    let service = ServiceBuilder::new()
        .layer(tunnelbanna)
        .layer(hide_special_files)
        .service(serve_dir);

    let listener = TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("Failed to bind to port 8080");

    let server = ConnBuilder::new(TokioExecutor::new());
    let graceful = GracefulShutdown::new();
    let mut ctrl_c = pin!(vss::shutdown_signal());

    loop {
        tokio::select! {
            conn = listener.accept() => {
                match conn {
                    Ok((stream, peer_addr)) => {
                        info!("incoming connection accepted: {}", peer_addr);
                        let stream = hyper_util::rt::TokioIo::new(Box::pin(stream));

                        let conn = server.serve_connection_with_upgrades(stream, TowerToHyperService::new(service));

                        let conn = graceful.watch(conn.into_owned());

                        tokio::spawn(async move {
                            if let Err(err) = conn.await {
                                eprintln!("connection error: {}", err);
                            }
                            eprintln!("connection dropped: {}", peer_addr);
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
                eprintln!("Ctrl-C received, starting shutdown");
                break;
            }
        }
    }

    tokio::select! {
        _ = graceful.shutdown() => {
            eprintln!("Gracefully shutdown!");
        },
        _ = tokio::time::sleep(Duration::from_secs(10)) => {
            eprintln!("Waited 10 seconds for graceful shutdown, aborting...");
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
