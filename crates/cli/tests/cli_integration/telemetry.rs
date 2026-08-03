// SPDX-License-Identifier: Apache-2.0
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::Duration,
};

use super::*;

const OTEL_ENV: &[&str] = &[
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
];

#[test]
fn opted_in_real_command_reaches_otlp_http_collector() {
    let repo = TempDir::new().unwrap();
    let init = heddle_output_with_env_removed(&["init"], Some(repo.path()), &[], OTEL_ENV).unwrap();
    assert!(init.status.success());

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (captured_tx, captured_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("OTLP request");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request = read_http_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .unwrap();
        captured_tx.send(request).unwrap();
    });

    let output = heddle_output_with_env_removed(
        &["--output", "json", "status"],
        Some(repo.path()),
        &[("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint.as_str())],
        OTEL_ENV,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let request = captured_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("collector receives an export request");
    server.join().unwrap();
    let header_end = find_bytes(&request, b"\r\n\r\n").expect("HTTP headers") + 4;
    let headers = String::from_utf8_lossy(&request[..header_end]);
    assert!(headers.starts_with("POST /v1/traces HTTP/1.1"), "{headers}");
    assert!(
        request[header_end..]
            .windows(b"heddle.command".len())
            .any(|window| window == b"heddle.command"),
        "OTLP protobuf body should contain the command span name"
    );
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).expect("read OTLP request headers");
        assert_ne!(read, 0, "OTLP request ended before headers");
        request.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_bytes(&request, b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
        })
        .expect("OTLP request content-length");
    while request.len() < header_end + content_length {
        let read = stream.read(&mut chunk).expect("read OTLP request body");
        assert_ne!(read, 0, "OTLP request body was truncated");
        request.extend_from_slice(&chunk[..read]);
    }
    request
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
