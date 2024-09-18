use std::{convert::Infallible, fs, net::SocketAddr, path::Path};

use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server, StatusCode,
};

async fn serve_file(req: Request<Body>) -> Result<Response<Body>, Infallible> {
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
                .body(Body::from(contents))
                .unwrap())
        }
        Err(_) => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("404 Not Found"))
            .unwrap()),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 3001));

    let make_svc = make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(serve_file)) });

    let server = Server::bind(&addr).serve(make_svc);

    println!("Listening on http://{}", addr);

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}
