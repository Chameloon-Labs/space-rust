use std::io::{self, Read};
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::thread;

use http::Request;

use crate::Server;

/// Biggest request body the bundled server reads.
const MAX_BODY: u64 = 16 * 1024 * 1024;

/// Runs the bundled HTTP server for `server` on `addr`, one thread per request. Does not return.
pub fn serve(server: Arc<Server>, addr: impl ToSocketAddrs) -> io::Result<()> {
    let listener = tiny_http::Server::http(addr).map_err(io::Error::other)?;
    for request in listener.incoming_requests() {
        let server = server.clone();
        thread::spawn(move || answer(&server, request));
    }
    Ok(())
}

fn answer(server: &Server, mut request: tiny_http::Request) {
    let mut body = Vec::new();
    let read = request.as_reader().take(MAX_BODY).read_to_end(&mut body);
    let mut builder = Request::builder()
        .method(request.method().as_str())
        .uri(request.url());
    for h in request.headers() {
        builder = builder.header(h.field.as_str().as_str(), h.value.as_str());
    }
    let reply = match (read, builder.body(body)) {
        (Ok(_), Ok(req)) => server.handle(req),
        _ => http::Response::builder()
            .status(400)
            .body(b"bad request".to_vec())
            .unwrap(),
    };
    let (parts, body) = reply.into_parts();
    let mut response = tiny_http::Response::from_data(body).with_status_code(parts.status.as_u16());
    for (name, value) in &parts.headers {
        if let Ok(h) = tiny_http::Header::from_bytes(name.as_str().as_bytes(), value.as_bytes()) {
            response.add_header(h);
        }
    }
    let _ = request.respond(response);
}
