//! The local API client must report a redirect, not follow it.
//!
//! A followed redirect would replay a prompt against whatever the server names,
//! so the client is built with redirects disabled. The test server accepts one
//! connection, answers 307 with a same-host location, and fails if a second
//! connection arrives.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use ntfy_relay_opencode::api::{Api, BaseUrl, Delivery, HttpApi};

#[tokio::test]
async fn a_redirect_is_reported_and_not_followed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 4096];
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0, "the client sent a request");
        stream
            .write_all(
                b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        listener.accept().is_err()
    });

    let base = BaseUrl::parse(&format!("http://{address}")).unwrap();
    let api = HttpApi::new(base).unwrap();
    let error = api
        .prompt("ses_1", "hello", Delivery::Queue, &[])
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(307), "{}", error.message());
    assert!(server.join().unwrap(), "the redirect target was requested");
}
