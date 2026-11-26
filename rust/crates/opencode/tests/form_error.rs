//! A failed form cancel must not echo the answer.
//!
//! The answer travels in the query string, which the server needs; the client
//! error text and the log line must name the route only.

use std::io::{Read, Write};
use std::net::TcpListener;

use ntfy_relay_opencode::api::{Api, BaseUrl, HttpApi};

#[tokio::test]
async fn a_failed_cancel_names_the_route_and_not_the_answer() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 4096];
        let read = stream.read(&mut buffer).unwrap();
        stream
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        String::from_utf8_lossy(&buffer[..read]).into_owned()
    });

    let base = BaseUrl::parse(&format!("http://{address}")).unwrap();
    let api = HttpApi::new(base).unwrap();
    let error = api
        .cancel_form("ses_1", "frm_1", "hunter2-secret")
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(500), "{}", error.message());
    assert!(
        error
            .message()
            .starts_with("DELETE /api/session/ses_1/form/frm_1:"),
        "{}",
        error.message()
    );
    assert!(
        !error.message().contains("hunter2-secret"),
        "{}",
        error.message()
    );

    let request = server.join().unwrap();
    assert!(request.starts_with("DELETE /api/session/ses_1/form/frm_1?"));
    assert!(request.contains("message=hunter2-secret"), "{request}");
}
