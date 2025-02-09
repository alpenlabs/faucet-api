use std::{convert::Infallible, error::Error, fs, net::SocketAddr, path::Path};

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::{body::Incoming, service::service_fn, Request, Response, StatusCode};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use tokio::{net::TcpListener, task::JoinSet};

async fn serve_file(
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, Infallible>>, Infallible> {
    let path = match req.uri().path() {
        "/" => "static/index.html",
        path => &path[1..], // strip leading '/'
    };

    let file_path = Path::new(path);

    match fs::read(file_path) {
        Ok(contents) => {
            let mime_type = match file_path.extension().and_then(|ext| ext.to_str()) {
                Some("html") => "text/html",
                Some("js") => "application/javascript",
                _ => "application/octet-stream",
            };

            Ok(Response::builder()
                .header("Content-Type", mime_type)
                .body(Full::from(contents).boxed())
                .unwrap())
        }
        Err(_) => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("404 Not Found\n")).boxed())
            .unwrap()),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    let listen_addr = SocketAddr::from(([127, 0, 0, 1], 3001));
    let tcp_listener = TcpListener::bind(&listen_addr).await?;
    println!("listening on http://{listen_addr}");

    let mut join_set = JoinSet::new();
    loop {
        let (stream, addr) = match tcp_listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("failed to accept connection: {e}");
                continue;
            }
        };

        let serve_connection = async move {
            println!("handling a request from {addr}");

            let result = Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service_fn(serve_file))
                .await;

            if let Err(e) = result {
                eprintln!("error serving {addr}: {e}");
            }

            println!("handled a request from {addr}");
        };

        join_set.spawn(serve_connection);
    }
}
