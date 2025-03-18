#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
//! # tunnelbana
//!
//! tunnelbana is a binary which uses the [tunnelbana project](https://github.com/randomairborne/tunnelbana)
//! to build a static file server.
use std::{
    io::{Error as IoError, ErrorKind as IoErrorKind},
    path::{Path, PathBuf},
    pin::pin,
    process::{ExitCode, Termination},
    time::Duration,
};

use futures_util::future::Either;
use http::{HeaderValue, StatusCode};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::{conn::auto::Builder as ConnBuilder, graceful::GracefulShutdown},
    service::TowerToHyperService,
};
use tokio::{net::TcpListener, runtime::Builder as RuntimeBuilder};
use tokio_util::task::TaskTracker;
use tower::ServiceBuilder;
use tower_http::{
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
    set_status::SetStatusLayer,
};
use tracing::Level;
use tunnelbana_etags::{ETagLayer, ETagMap};
use tunnelbana_headers::HeadersLayer;
use tunnelbana_redirects::RedirectsLayer;

#[macro_use]
extern crate tracing;

const RESERVED_PATHS: [&str; 2] = ["/_headers", "/_redirects"];

#[cfg(debug_assertions)]
const LOG_LEVEL: Level = Level::TRACE;

#[cfg(not(debug_assertions))]
const LOG_LEVEL: Level = Level::INFO;

use argh::FromArgs;

#[derive(FromArgs)]
/// Serve a directory
struct Args {
    /// fall back to index.html rather than 404.html
    #[argh(switch)]
    spa: bool,

    /// directory to serve
    #[argh(positional)]
    directory: PathBuf,
}

#[derive(Debug)]
struct Error {
    msg: &'static str,
    inner: Option<Box<dyn std::error::Error>>,
    file: &'static str,
    line: u32,
    column: u32,
}

macro_rules! e {
    ($str:literal) => {
        $crate::Error {
            msg: $str,
            inner: None,
            file: std::file!(),
            line: std::line!(),
            column: std::column!(),
        }
    };
    ($str:literal, $e:expr) => {
        $crate::Error {
            msg: $str,
            inner: Some(Box::new($e)),
            file: std::file!(),
            line: std::line!(),
            column: std::column!(),
        }
    };
}

impl Termination for Error {
    fn report(self) -> ExitCode {
        if let Some(inner_err) = self.inner {
            eprintln!("{inner_err}")
        }
        eprintln!(
            "{} at {}:{}:{}",
            self.msg, self.file, self.line, self.column
        );
        ExitCode::FAILURE
    }
}

const CACHE_CONTROL_TEXT: &str = "no-transform";
static CACHE_CONTRL_VALUE: HeaderValue = HeaderValue::from_static(CACHE_CONTROL_TEXT);

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Error> {
    tracing_subscriber::fmt().with_max_level(LOG_LEVEL).init();
    let args: Args = argh::from_env();
    let location = Path::new(&args.directory);
    if !location.is_dir() {
        return Err(e!("Expected argument 1 to be a directory"));
    }
    let location = location
        .canonicalize()
        .map_err(|e| e!("Could not canonicalize directory", e))?;

    let headers = read_with_default_if_nonexistent(location.join("_headers"))
        .map_err(|e| e!("Failed to read _headers", e))?;
    let headers =
        tunnelbana_headers::parse(&headers).map_err(|e| e!("Failed to parse _headers", e))?;

    let redirects = read_with_default_if_nonexistent(location.join("_redirects"))
        .map_err(|e| e!("Failed to read _redirects", e))?;
    let redirects =
        tunnelbana_redirects::parse(&redirects).map_err(|e| e!("Failed to parse _redirects", e))?;

    let etags = ETagMap::new(&location).map_err(|e| e!("Failed to generate etags", e))?;

    let redirect_mw =
        RedirectsLayer::new(redirects).map_err(|e| e!("Failed to build redirects router", e))?;
    let header_add_mw =
        HeadersLayer::new(headers).map_err(|e| e!("Failed to build headers router", e))?;

    let etag_mw = ETagLayer::new(etags);

    let (not_found_path, not_found_status_layer) = if args.spa {
        ("index.html", None)
    } else {
        ("404.html", Some(SetStatusLayer::new(StatusCode::NOT_FOUND)))
    };

    let not_found_svc = ServeFile::new(location.join(not_found_path))
        .precompressed_br()
        .precompressed_deflate()
        .precompressed_gzip()
        .precompressed_zstd();
    let not_found_svc = ServiceBuilder::new()
        .option_layer(not_found_status_layer)
        .service(not_found_svc);
    let serve_dir = ServeDir::new(location)
        .append_index_html_on_directories(true)
        .precompressed_br()
        .precompressed_deflate()
        .precompressed_gzip()
        .precompressed_zstd()
        .fallback(not_found_svc.clone());

    let hide_special_files = tunnelbana_hidepaths::HidePathsLayer::builder()
        .hide_all(RESERVED_PATHS)
        .with_not_found_service(not_found_svc)
        .build()
        .map_err(|e| e!("Failed to build path hide layer", e))?;

    let set_vary = SetResponseHeaderLayer::appending(
        http::header::VARY,
        HeaderValue::from_name(http::header::ACCEPT_ENCODING),
    );

    let set_cache_control =
        SetResponseHeaderLayer::appending(http::header::CACHE_CONTROL, CACHE_CONTRL_VALUE.clone());

    let service = ServiceBuilder::new()
        .layer(header_add_mw)
        .layer(redirect_mw)
        .layer(etag_mw)
        .layer(hide_special_files)
        .layer(set_vary)
        .layer(set_cache_control)
        .service(serve_dir);

    let rt = RuntimeBuilder::new_current_thread()
        .enable_all()
        .thread_name("tunnelbana-worker")
        .build()
        .map_err(|e| e!("Invalid runtime config", e))?;

    let listener = rt
        .block_on(TcpListener::bind("0.0.0.0:8080"))
        .map_err(|e| e!("Failed to bind to port 8080", e))?;

    let server = ConnBuilder::new(TokioExecutor::new());
    let graceful = GracefulShutdown::new();
    let tasks = TaskTracker::new();
    let ctrl_c = vss::shutdown_signal();

    let main_task = rt.spawn(async move {
        let mut ctrl_c = pin!(ctrl_c);
        loop {
            let service = service.clone();
            let listener_fut = pin!(listener.accept());
            let selected = futures_util::future::select(listener_fut, ctrl_c.as_mut()).await;
            let Either::Left((conn, _)) = selected else {
                info!("Ctrl-C received, starting shutdown");
                break;
            };
            let (stream, peer_addr) = match conn {
                Ok(v) => v,
                Err(e) => {
                    warn!("accept error: {}", e);
                    continue;
                }
            };
            info!("incoming connection accepted: {}", peer_addr);
            let stream = TokioIo::new(Box::pin(stream));

            let conn = server
                .serve_connection_with_upgrades(stream, TowerToHyperService::new(service))
                .into_owned();
            let conn = graceful.watch(conn.into_owned());

            tasks.spawn(async move {
                if let Err(err) = conn.await {
                    warn!("connection error: {}", err);
                }
                debug!("connection dropped: {}", peer_addr);
            });
        }
        shut_down(graceful, tasks).await;
    });

    rt.block_on(main_task)
        .map_err(|e| e!("Background task failed", e))?;
    Ok(())
}

async fn shut_down(graceful: GracefulShutdown, tasks: TaskTracker) {
    const SHUTDOWN_GRACEFUL_DEADLINE: Duration = Duration::from_secs(5);
    match futures_util::future::select(
        pin!(graceful.shutdown()),
        pin!(tokio::time::sleep(SHUTDOWN_GRACEFUL_DEADLINE)),
    )
    .await
    {
        Either::Left(_) => {
            info!("Gracefully shutdown!");
        }
        Either::Right(_) => {
            error!("Waited 10 seconds for graceful shutdown, aborting...");
            return;
        }
    }

    tasks.close();

    match futures_util::future::select(
        pin!(tasks.wait()),
        pin!(tokio::time::sleep(SHUTDOWN_GRACEFUL_DEADLINE)),
    )
    .await
    {
        Either::Left(_) => {
            info!("Gracefully shutdown!");
        }
        Either::Right(_) => {
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
