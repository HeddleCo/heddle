// SPDX-License-Identifier: Apache-2.0
//! The single-request HTTP and subprocess boundary for the bounded gateway.

use crate::Result;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub const MAX_REQUEST: usize = 1024 * 1024;
pub const MAX_PACK: usize = 16 * 1024 * 1024;
pub const MAX_RECEIVE_REQUEST: usize = MAX_PACK + 64 * 1024;
pub const MAX_RESPONSE: u64 = 96 * 1024 * 1024;
const MAX_HEADERS: usize = 16 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Request {
    pub stream: TcpStream,
    pub method: String,
    pub pin: String,
    pub endpoint: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn reply(&mut self, status: u16, body: &[u8]) -> Result<()> {
        reply_stream(&mut self.stream, status, body)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "gateway I/O deadline"))
}

fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .map(|n| n + 4)
}

fn strict_lines(bytes: &[u8]) -> io::Result<()> {
    for (index, byte) in bytes.iter().enumerate() {
        if (*byte == b'\n' && (index == 0 || bytes[index - 1] != b'\r'))
            || (*byte == b'\r' && bytes.get(index + 1) != Some(&b'\n'))
        {
            return Err(invalid("HTTP requires CRLF line endings"));
        }
    }
    Ok(())
}

fn decimal(value: &str) -> io::Result<usize> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("invalid content length"));
    }
    value.parse().map_err(|_| invalid("invalid content length"))
}

fn owned_headers(headers: &[httparse::Header<'_>]) -> io::Result<Vec<(String, String)>> {
    let mut names = HashSet::with_capacity(headers.len());
    let mut result = Vec::with_capacity(headers.len());
    for header in headers {
        let name = header.name.to_ascii_lowercase();
        // One spelling and one value per field: no first/last/comma-joined
        // interpretation can disagree across the authentication boundary.
        if !names.insert(name.clone()) {
            return Err(invalid("duplicate HTTP header"));
        }
        if !header.value.iter().all(|b| (b' '..=b'~').contains(b)) {
            return Err(invalid("non-ASCII or control character in HTTP header"));
        }
        let value = std::str::from_utf8(header.value)
            .map_err(|_| invalid("invalid HTTP header"))?
            .to_owned();
        result.push((name, value));
    }
    Ok(result)
}

struct Head {
    method: String,
    pin: String,
    endpoint: String,
    query: String,
    headers: Vec<(String, String)>,
    length: usize,
}

fn parse_head(bytes: &[u8], allowed_host: &str) -> Result<Head> {
    parse_head_for(bytes, allowed_host, None)
}

fn parse_window_head(bytes: &[u8], allowed_host: &str, repository_name: &str) -> Result<Head> {
    if !(1..=64).contains(&repository_name.len())
        || !repository_name.as_bytes()[0].is_ascii_alphanumeric()
        || !repository_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(invalid("invalid window repository name").into());
    }
    parse_head_for(bytes, allowed_host, Some(repository_name))
}

fn parse_head_for(bytes: &[u8], allowed_host: &str, repository_name: Option<&str>) -> Result<Head> {
    if bytes.len() > MAX_HEADERS || !bytes.ends_with(b"\r\n\r\n") {
        return Err(invalid("HTTP header limit").into());
    }
    strict_lines(bytes)?;
    if bytes.starts_with(b"\r\n") {
        return Err(invalid("empty request line").into());
    }
    let mut slots = [httparse::EMPTY_HEADER; 128];
    let mut parsed = httparse::Request::new(&mut slots);
    if parsed.parse(bytes)? != httparse::Status::Complete(bytes.len()) || parsed.version != Some(1)
    {
        return Err(invalid("complete HTTP/1.1 request required").into());
    }
    let method = parsed.method.ok_or_else(|| invalid("missing method"))?;
    let target = parsed.path.ok_or_else(|| invalid("missing target"))?;
    let headers = owned_headers(parsed.headers)?;
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let host = header("host").ok_or_else(|| invalid("missing host"))?;
    if host != allowed_host
        && !(allowed_host == "native-container.invalid" && host == "native-container.invalid:8080")
    {
        return Err(invalid("unapproved host").into());
    }
    for (name, _) in &headers {
        if matches!(
            name.as_str(),
            "origin" | "forwarded" | "x-real-ip" | "x-demo-reader"
        ) || name.starts_with("x-forwarded-")
            || name.starts_with("proxy-")
        {
            return Err(invalid("proxy or fixture header refused").into());
        }
        if matches!(
            name.as_str(),
            "transfer-encoding" | "content-encoding" | "expect" | "upgrade"
        ) {
            return Err(invalid("encoded or ambiguous HTTP request refused").into());
        }
    }
    if header("authorization").is_none() || header("x-gateway-service-authorization").is_none() {
        return Err(invalid("independent reader and service identities required").into());
    }
    if let Some(protocol) = header("git-protocol")
        && !matches!(protocol, "version=1" | "version=2")
    {
        return Err(invalid("unsupported Git protocol").into());
    }
    let (pin, rest) = if let Some(name) = repository_name {
        let prefix = format!("/repositories/{name}.git/");
        let rest = target
            .strip_prefix(&prefix)
            .ok_or_else(|| invalid("unavailable window route"))?;
        (name, rest)
    } else {
        let route = target
            .strip_prefix("/views/")
            .ok_or_else(|| invalid("unavailable route"))?;
        let (pin, rest) = route
            .split_once(".git/")
            .ok_or_else(|| invalid("unavailable route"))?;
        if pin.len() != 40
            || !pin
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid("invalid view pin").into());
        }
        (pin, rest)
    };
    let (endpoint, query, content_type, maximum) = match (method, rest) {
        ("GET", "info/refs?service=git-upload-pack") => {
            ("info/refs", "service=git-upload-pack", "", MAX_REQUEST)
        }
        ("POST", "git-upload-pack") => (
            "git-upload-pack",
            "",
            "application/x-git-upload-pack-request",
            MAX_REQUEST,
        ),
        ("GET", "info/refs?service=git-receive-pack") if repository_name.is_some() => (
            "info/refs",
            "service=git-receive-pack",
            "",
            MAX_RECEIVE_REQUEST,
        ),
        ("POST", "git-receive-pack") if repository_name.is_some() => (
            "git-receive-pack",
            "",
            "application/x-git-receive-pack-request",
            MAX_RECEIVE_REQUEST,
        ),
        _ => return Err(invalid("read-only smart HTTP route required").into()),
    };
    let length = header("content-length")
        .map(decimal)
        .transpose()?
        .unwrap_or(0);
    if length > maximum {
        return Err(invalid("request body limit").into());
    }
    if method == "GET" && length != 0 {
        return Err(invalid("GET request body refused").into());
    }
    if method == "POST"
        && (header("content-type") != Some(content_type) || header("content-length").is_none())
    {
        return Err(invalid("explicit Git request content type and length required").into());
    }
    Ok(Head {
        method: method.to_owned(),
        pin: pin.to_owned(),
        endpoint: endpoint.to_owned(),
        query: query.to_owned(),
        headers,
        length,
    })
}

/// Read precisely one request. The caller closes the connection after replying.
/// `allowed_host` is exact; the production host additionally permits `:8080`.
pub fn read_request(stream: TcpStream, allowed_host: &str) -> Result<Request> {
    read_request_with(stream, |bytes| parse_head(bytes, allowed_host))
}

/// The local mutable-window caller opts in to these routes explicitly. This
/// does not enable receive-pack on the immutable gateway or execute Git writes.
pub fn read_window_request(
    stream: TcpStream,
    allowed_host: &str,
    repository_name: &str,
) -> Result<Request> {
    read_request_with(stream, |bytes| {
        parse_window_head(bytes, allowed_host, repository_name)
    })
}

/// Internal Worker-to-native bridge. Authenticate the independent service
/// before allocating the bounded raw frame; it never grants Git user authority.
#[cfg(feature = "gateway-publication")]
pub fn read_native_request(
    stream: TcpStream,
    allowed_host: &str,
    service_sha256: &str,
) -> Result<Request> {
    read_request_with(stream, |bytes| {
        strict_lines(bytes)?;
        if bytes.len() > MAX_HEADERS || !bytes.ends_with(b"\r\n\r\n") {
            return Err(invalid("native HTTP header limit").into());
        }
        let mut slots = [httparse::EMPTY_HEADER; 128];
        let mut parsed = httparse::Request::new(&mut slots);
        if parsed.parse(bytes)? != httparse::Status::Complete(bytes.len())
            || parsed.version != Some(1)
            || parsed.method != Some("POST")
            || parsed.path != Some("/native/v1")
        {
            return Err(invalid("native bridge route refused").into());
        }
        let headers = owned_headers(parsed.headers)?;
        let header = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        if (header("host") != Some(allowed_host)
            && !(allowed_host == "native-container.invalid"
                && header("host") == Some("native-container.invalid:8080")))
            || header("content-type") != Some(crate::native_frame::CONTENT_TYPE)
        {
            return Err(invalid("native bridge host or content type").into());
        }
        for (name, _) in &headers {
            if matches!(
                name.as_str(),
                "transfer-encoding"
                    | "content-encoding"
                    | "expect"
                    | "upgrade"
                    | "origin"
                    | "forwarded"
            ) || name.starts_with("x-forwarded-")
                || name.starts_with("proxy-")
            {
                return Err(invalid("ambiguous native request").into());
            }
        }
        let secret = header("x-gateway-service-authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| invalid("native service authentication required"))?;
        if !(32..=256).contains(&secret.len())
            || !secret.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
            || !crate::policy::hex(service_sha256, 64)
            || crate::policy::digest(secret.as_bytes())
                .bytes()
                .zip(service_sha256.bytes())
                .fold(0u8, |a, (b, c)| a | (b ^ c))
                != 0
        {
            return Err(invalid("native service denied").into());
        }
        if header("authorization").is_none() {
            return Err(invalid("Git session required").into());
        }
        let length =
            decimal(header("content-length").ok_or_else(|| invalid("native length required"))?)?;
        // Raw source/proof parts share one exact bounded native frame.
        if length == 0 || length > crate::native_frame::REQUEST_MAX {
            return Err(invalid("native body limit").into());
        }
        Ok(Head {
            method: "POST".into(),
            pin: String::new(),
            endpoint: "native".into(),
            query: String::new(),
            headers,
            length,
        })
    })
}

#[cfg(feature = "gateway-publication")]
pub fn reply_native_frame(request: &mut Request, frame: crate::native_frame::Frame) -> Result<()> {
    // All lengths, markers and caps are validated before the HTTP status line.
    let (prefix, length) = frame.encoded_header(crate::native_frame::RESPONSE_MAX)?;
    let deadline = Instant::now() + IO_TIMEOUT;
    let result = (|| {
        write_before(
            &mut request.stream,
            &response_head(200, length as u64, crate::native_frame::CONTENT_TYPE)?,
            deadline,
        )?;
        write_before(&mut request.stream, &prefix, deadline)?;
        for bytes in frame.parts.values() {
            write_before(&mut request.stream, bytes, deadline)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = request.stream.shutdown(Shutdown::Both);
    }
    result
}

fn read_request_with(
    mut stream: TcpStream,
    parse: impl FnOnce(&[u8]) -> Result<Head>,
) -> Result<Request> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut raw = Vec::with_capacity(4096);
    let offset = loop {
        if let Some(offset) = header_end(&raw) {
            if offset > MAX_HEADERS {
                return Err(invalid("HTTP header limit").into());
            }
            break offset;
        }
        if raw.len() >= MAX_HEADERS {
            return Err(invalid("HTTP header limit").into());
        }
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err(invalid("incomplete HTTP headers").into());
        }
        raw.extend_from_slice(&chunk[..count]);
    };
    let head = parse(&raw[..offset])?;
    let mut body = raw.split_off(offset);
    if body.len() > head.length {
        return Err(invalid("bytes beyond declared request body").into());
    }
    body.reserve(head.length - body.len());
    while body.len() < head.length {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let mut chunk = [0_u8; 16 * 1024];
        let wanted = chunk.len().min(head.length - body.len());
        let count = stream.read(&mut chunk[..wanted])?;
        if count == 0 {
            return Err(invalid("incomplete HTTP body").into());
        }
        body.extend_from_slice(&chunk[..count]);
    }
    Ok(Request {
        stream,
        method: head.method,
        pin: head.pin,
        endpoint: head.endpoint,
        query: head.query,
        headers: head.headers,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Gateway Response",
    }
}

fn write_before(stream: &mut TcpStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        let count = stream.write(bytes)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "HTTP response closed",
            ));
        }
        bytes = &bytes[count..];
    }
    Ok(())
}

fn response_head(status: u16, length: u64, content_type: &str) -> Result<Vec<u8>> {
    if !(200..=599).contains(&status) || !content_type.bytes().all(|b| (b' '..=b'~').contains(&b)) {
        return Err(invalid("invalid response headers").into());
    }
    Ok(format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
        reason(status),
    ).into_bytes())
}

pub fn reply_stream(stream: &mut TcpStream, status: u16, body: &[u8]) -> Result<()> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let head = response_head(status, body.len() as u64, "text/plain; charset=utf-8")?;
    write_before(stream, &head, deadline)?;
    write_before(stream, body, deadline)?;
    Ok(())
}

/// Send a completed in-memory Git advertisement or status after the caller's
/// final authority check. A failed send closes the stream before any fallback.
pub fn reply_git(request: &mut Request, content_type: &str, body: &[u8]) -> Result<()> {
    if body.len() as u64 > MAX_RESPONSE {
        return Err(invalid("Git response limit").into());
    }
    if !matches!(
        content_type,
        "application/x-git-upload-pack-advertisement"
            | "application/x-git-upload-pack-result"
            | "application/x-git-receive-pack-advertisement"
            | "application/x-git-receive-pack-result"
    ) {
        return Err(invalid("unexpected Git content type").into());
    }
    let head = response_head(200, body.len() as u64, content_type)?;
    let deadline = Instant::now() + IO_TIMEOUT;
    let result = write_before(&mut request.stream, &head, deadline)
        .and_then(|()| write_before(&mut request.stream, body, deadline));
    if result.is_err() {
        let _ = request.stream.shutdown(Shutdown::Both);
    }
    result.map_err(Into::into)
}

/// The CGI output is retained on disk until the caller's final authority check.
pub struct GitResponse {
    output: File,
    status: u16,
    content_type: String,
    body_offset: u64,
    body_length: u64,
}

impl GitResponse {
    /// A bounded internal bridge envelope remains private until the Worker has
    /// performed its final independent current-disclosure check.
    #[cfg(feature = "gateway-publication")]
    pub fn into_bytes(mut self) -> Result<(u16, String, Vec<u8>)> {
        if self.body_length > MAX_RESPONSE {
            return Err(invalid("Git response limit").into());
        }
        self.output.seek(SeekFrom::Start(self.body_offset))?;
        let mut bytes = Vec::new();
        self.output
            .take(self.body_length + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != self.body_length {
            return Err(invalid("truncated Git response").into());
        }
        Ok((self.status, self.content_type, bytes))
    }
    pub fn send(mut self, request: &mut Request) -> Result<()> {
        let result = self.send_body(&mut request.stream);
        if result.is_err() {
            // The caller may try a generic error reply. It must never append
            // another status line to a partially transmitted Git response.
            let _ = request.stream.shutdown(Shutdown::Both);
        }
        result
    }

    fn send_body(&mut self, stream: &mut TcpStream) -> Result<()> {
        let deadline = Instant::now() + IO_TIMEOUT;
        self.output.seek(SeekFrom::Start(self.body_offset))?;
        let head = response_head(self.status, self.body_length, &self.content_type)?;
        write_before(stream, &head, deadline)?;
        let mut remaining = self.body_length;
        let mut chunk = [0_u8; 32 * 1024];
        while remaining > 0 {
            let wanted = chunk.len().min(remaining as usize);
            let count = self.output.read(&mut chunk[..wanted])?;
            if count == 0 {
                return Err(invalid("truncated Git response").into());
            }
            write_before(stream, &chunk[..count], deadline)?;
            remaining -= count as u64;
        }
        Ok(())
    }
}

fn parse_git_response(mut output: File, endpoint: &str) -> Result<GitResponse> {
    let expected = match endpoint {
        "info/refs" => "application/x-git-upload-pack-advertisement",
        "git-upload-pack" => "application/x-git-upload-pack-result",
        _ => return Err(invalid("read-only Git backend required").into()),
    };
    let size = output.metadata()?.len();
    if size > MAX_RESPONSE {
        return Err(invalid("Git response limit").into());
    }
    output.seek(SeekFrom::Start(0))?;
    let mut raw = Vec::with_capacity(4096);
    let offset = loop {
        if let Some(offset) = header_end(&raw) {
            break offset;
        }
        if raw.len() >= MAX_HEADERS {
            return Err(invalid("CGI header limit").into());
        }
        let mut chunk = [0_u8; 4096];
        let count = output.read(&mut chunk)?;
        if count == 0 {
            return Err(invalid("incomplete CGI response").into());
        }
        raw.extend_from_slice(&chunk[..count]);
    };
    if offset > MAX_HEADERS {
        return Err(invalid("CGI header limit").into());
    }
    strict_lines(&raw[..offset])?;
    let mut slots = [httparse::EMPTY_HEADER; 128];
    let headers = match httparse::parse_headers(&raw[..offset], &mut slots)? {
        httparse::Status::Complete((length, headers)) if length == offset => {
            owned_headers(headers)?
        }
        _ => return Err(invalid("invalid CGI headers").into()),
    };
    let mut status = 200;
    let mut content_type = None;
    for (name, value) in headers {
        match name.as_str() {
            "status" => {
                let code = value
                    .split(' ')
                    .next()
                    .ok_or_else(|| invalid("invalid CGI status"))?;
                if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid("invalid CGI status").into());
                }
                status = code.parse::<u16>()?;
                if !(200..=599).contains(&status) {
                    return Err(invalid("invalid CGI status").into());
                }
            }
            "content-type" => content_type = Some(value),
            "content-length" if decimal(&value)? as u64 != size - offset as u64 => {
                return Err(invalid("inconsistent CGI length").into());
            }
            "transfer-encoding" | "content-encoding" => {
                return Err(invalid("encoded CGI response refused").into());
            }
            _ => {}
        }
    }
    let content_type = content_type.ok_or_else(|| invalid("missing CGI content type"))?;
    if status == 200 && content_type != expected {
        return Err(invalid("unexpected Git content type").into());
    }
    Ok(GitResponse {
        output,
        status,
        content_type,
        body_offset: offset as u64,
        body_length: size - offset as u64,
    })
}

/// Do not send until the caller has rechecked current reader/service authority.
pub fn prepare_git(request: &Request, repo: &Path) -> Result<GitResponse> {
    if !matches!(
        (
            request.method.as_str(),
            request.endpoint.as_str(),
            request.query.as_str()
        ),
        ("GET", "info/refs", "service=git-upload-pack") | ("POST", "git-upload-pack", "")
    ) {
        return Err(invalid("read-only Git backend required").into());
    }
    let repo = repo.canonicalize()?;
    let parent = repo
        .parent()
        .ok_or_else(|| invalid("invalid Git repository path"))?;
    let name = repo
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("invalid Git repository name"))?;
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(invalid("invalid Git repository name").into());
    }
    let mut command = Command::new("git");
    command
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", parent)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", format!("/{name}/{}", request.endpoint))
        .env("QUERY_STRING", &request.query)
        .env("REQUEST_METHOD", &request.method)
        .env("CONTENT_TYPE", request.header("content-type").unwrap_or(""))
        .env("CONTENT_LENGTH", request.body.len().to_string())
        .env("REMOTE_ADDR", "127.0.0.1");
    if let Some(protocol @ ("version=1" | "version=2")) = request.header("git-protocol") {
        command.env("GIT_PROTOCOL", protocol);
    }
    let output = tempfile::tempfile()?;
    let status = run_bounded(&mut command, &request.body, output.try_clone()?)?;
    if !status.success() {
        return Err(invalid("Git backend failed").into());
    }
    parse_git_response(output, &request.endpoint)
}

fn clean_environment(command: &mut Command) {
    // Preserve only explicit CGI/projection inputs, never ambient Git config,
    // alternates, credentials, loaders, proxies, or user HOME configuration.
    let cgi: Vec<_> = command
        .get_envs()
        .filter_map(|(key, value)| {
            let name = key.to_str()?;
            if matches!(
                name,
                "GIT_PROJECT_ROOT"
                    | "GIT_HTTP_EXPORT_ALL"
                    | "PATH_INFO"
                    | "QUERY_STRING"
                    | "REQUEST_METHOD"
                    | "CONTENT_TYPE"
                    | "CONTENT_LENGTH"
                    | "REMOTE_ADDR"
                    | "GIT_PROTOCOL"
                    | "HEDDLE_HOME"
                    | "HEDDLE_PRINCIPAL_NAME"
                    | "HEDDLE_PRINCIPAL_EMAIL"
            ) {
                value.map(|value| (key.to_owned(), value.to_owned()))
            } else {
                None
            }
        })
        .collect();
    command
        .env_clear()
        .envs([
            ("PATH", "/usr/bin:/bin"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_AUTHOR_NAME", "Synthetic Catalog"),
            ("GIT_AUTHOR_EMAIL", "catalog@example.invalid"),
            ("GIT_COMMITTER_NAME", "Synthetic Catalog"),
            ("GIT_COMMITTER_EMAIL", "catalog@example.invalid"),
            ("GIT_AUTHOR_DATE", "1700000000 +0000"),
            ("GIT_COMMITTER_DATE", "1700000000 +0000"),
            ("LC_ALL", "C"),
        ])
        .envs(cgi);
}

#[cfg(target_os = "linux")]
fn child_limits(command: &mut Command) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let requested = [
        (libc::RLIMIT_CPU, 25),
        (libc::RLIMIT_AS, 1024 * 1024 * 1024),
        (libc::RLIMIT_FSIZE, MAX_RESPONSE),
        (libc::RLIMIT_NOFILE, 128),
        (libc::RLIMIT_CORE, 0),
    ];
    let mut limits = Vec::with_capacity(requested.len());
    for (resource, requested) in requested {
        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `current` is a valid, writable rlimit and resource is known.
        if unsafe { libc::getrlimit(resource, &mut current) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let limit = (requested as libc::rlim_t).min(current.rlim_max);
        limits.push((
            resource,
            libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            },
        ));
    }
    command.process_group(0);
    // SAFETY: the closure only iterates preallocated data and invokes setrlimit
    // between fork and exec. It does not allocate, log, or acquire Rust locks.
    unsafe {
        command.pre_exec(move || {
            for (resource, limit) in &limits {
                if libc::setrlimit(*resource, limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn child_limits(_command: &mut Command) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "bounded gateway requires Linux resource limits",
    ))
}

fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: this process group was created specifically for this child.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Execute without a shell, using file-backed stdin/stdout and a cleaned Git
/// environment. Only explicit CGI/projection variables survive. The caller checks status.
pub fn run_bounded(command: &mut Command, input: &[u8], mut stdout: File) -> Result<ExitStatus> {
    run_with_timeout(command, input, &mut stdout, CHILD_TIMEOUT, MAX_REQUEST)
}

/// The local window uses the same subprocess isolation for a bounded packfile.
/// This larger input allowance does not change the immutable request runner.
pub fn run_bounded_pack(
    command: &mut Command,
    input: &[u8],
    mut stdout: File,
) -> Result<ExitStatus> {
    run_with_timeout(command, input, &mut stdout, CHILD_TIMEOUT, MAX_PACK)
}

fn run_with_timeout(
    command: &mut Command,
    input: &[u8],
    stdout: &mut File,
    timeout: Duration,
    maximum_input: usize,
) -> Result<ExitStatus> {
    if input.len() > maximum_input || !stdout.metadata()?.is_file() {
        return Err(invalid("subprocess input or output limit").into());
    }
    let mut stdin = tempfile::tempfile()?;
    stdin.write_all(input)?;
    stdin.seek(SeekFrom::Start(0))?;
    stdout.set_len(0)?;
    stdout.seek(SeekFrom::Start(0))?;
    clean_environment(command);
    child_limits(command)?;
    command
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Any lingering descendants must not keep writing after the
                // parent exits and before we validate the completed output.
                kill_group(&mut child);
                if stdout.metadata()?.len() > MAX_RESPONSE {
                    return Err(invalid("subprocess output limit").into());
                }
                return Ok(status);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            Ok(None) => {
                kill_group(&mut child);
                return Err(
                    io::Error::new(io::ErrorKind::TimedOut, "Git subprocess deadline").into(),
                );
            }
            Err(error) => {
                kill_group(&mut child);
                return Err(error.into());
            }
        }
    }
}

fn bridge_url(origin: &str, path: &str) -> Result<reqwest::Url> {
    let origin = reqwest::Url::parse(origin)?;
    if !matches!(origin.scheme(), "http" | "https")
        || origin.host_str().is_none()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err(invalid("invalid fixed bridge origin").into());
    }
    let approved_path = if let Some(pin) = path
        .strip_prefix("/catalog/")
        .or_else(|| path.strip_prefix("/disclosure/"))
    {
        pin.len() == 40
            && pin
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    } else if let Some(source) = path.strip_prefix("/native/") {
        (1..=64).contains(&source.len())
            && source.as_bytes()[0].is_ascii_lowercase()
            && source
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    } else {
        false
    };
    if !approved_path {
        return Err(invalid("invalid fixed bridge path").into());
    }
    Ok(origin.join(path)?)
}

/// `client` must have redirects and proxies disabled. Each read has its own
/// deadline and only the fixed catalog/native/disclosure routes can be addressed.
pub fn bridge_read(
    client: &reqwest::blocking::Client,
    origin: &str,
    path: &str,
    maximum: usize,
) -> Result<Vec<u8>> {
    let url = bridge_url(origin, path)?;
    let mut response = client
        .get(url.clone())
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .timeout(Duration::from_secs(15))
        .send()?;
    if response.status() != reqwest::StatusCode::OK || response.url() != &url {
        return Err(invalid("bridge response unavailable").into());
    }
    let headers = response.headers();
    if headers
        .get_all(reqwest::header::CONTENT_TYPE)
        .iter()
        .count()
        != 1
        || headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            != Some("application/octet-stream")
        || headers.contains_key(reqwest::header::CONTENT_ENCODING)
        || headers
            .get_all(reqwest::header::CONTENT_LENGTH)
            .iter()
            .count()
            > 1
        || (headers.contains_key(reqwest::header::CONTENT_LENGTH)
            && headers.contains_key(reqwest::header::TRANSFER_ENCODING))
    {
        return Err(invalid("invalid bridge response headers").into());
    }
    let length = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .map(|v| {
            decimal(
                v.to_str()
                    .map_err(|_| invalid("invalid bridge content length"))?,
            )
        })
        .transpose()?;
    if length.is_some_and(|length| length > maximum) {
        return Err(invalid("bridge response limit").into());
    }
    let limit = maximum
        .checked_add(1)
        .ok_or_else(|| invalid("invalid bridge response limit"))?;
    let mut data = Vec::with_capacity(length.unwrap_or(0).min(maximum));
    response
        .by_ref()
        .take(limit as u64)
        .read_to_end(&mut data)?;
    if data.len() > maximum || length.is_some_and(|length| data.len() != length) {
        return Err(invalid("bridge response length mismatch").into());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    const PIN: &str = "0123456789abcdef0123456789abcdef01234567";
    const REPOSITORY: &str = "synthetic-demo";

    fn request(method: &str, suffix: &str, headers: &str) -> Vec<u8> {
        format!("{method} /views/{PIN}.git/{suffix} HTTP/1.1\r\nHost: native-container.invalid\r\nAuthorization: Bearer reader\r\nX-Gateway-Service-Authorization: Bearer service\r\n{headers}\r\n").into_bytes()
    }

    fn window_request(method: &str, suffix: &str, headers: &str) -> Vec<u8> {
        format!("{method} /repositories/{REPOSITORY}.git/{suffix} HTTP/1.1\r\nHost: native-container.invalid\r\nAuthorization: Bearer reader\r\nX-Gateway-Service-Authorization: Bearer service\r\n{headers}\r\n").into_bytes()
    }

    fn streams() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let peer = TcpStream::connect(listener.local_addr().expect("listener address"))
            .expect("loopback connection");
        let (stream, _) = listener.accept().expect("accepted connection");
        (stream, peer)
    }

    #[test]
    fn window_routes_opt_in_to_upload_and_receive_only_for_the_named_repository() {
        for service in ["git-upload-pack", "git-receive-pack"] {
            for method in ["GET", "POST"] {
                let suffix = if method == "GET" {
                    format!("info/refs?service={service}")
                } else {
                    service.to_owned()
                };
                let headers = if method == "POST" {
                    format!(
                        "Content-Type: application/x-{service}-request\r\nContent-Length: 4\r\n"
                    )
                } else {
                    String::new()
                };
                let raw = window_request(method, &suffix, &headers);
                let parsed = parse_window_head(&raw, "native-container.invalid", REPOSITORY)
                    .expect("explicit window route");
                assert_eq!(parsed.pin, REPOSITORY);
                assert_eq!(parsed.method, method);
                assert_eq!(
                    parsed.endpoint,
                    if method == "GET" {
                        "info/refs"
                    } else {
                        service
                    }
                );
                assert_eq!(
                    parsed.query,
                    if method == "GET" {
                        format!("service={service}")
                    } else {
                        String::new()
                    }
                );
                assert!(parse_head(&raw, "native-container.invalid").is_err());
                assert!(parse_window_head(&raw, "127.0.0.1:8042", REPOSITORY).is_err());
                assert!(parse_window_head(&raw, "native-container.invalid", "other").is_err());
                if service == "git-receive-pack" {
                    assert!(
                        parse_head(
                            &request(method, &suffix, &headers),
                            "native-container.invalid"
                        )
                        .is_err()
                    );
                }
            }
        }
        for (method, suffix) in [
            ("GET", "info/refs"),
            ("GET", "info/refs?service=git-receive-pack&extra=1"),
            ("GET", "info/refs?service=git-upload-pack#fragment"),
            ("GET", "info%2frefs?service=git-receive-pack"),
            ("GET", "../info/refs?service=git-receive-pack"),
            ("GET", "git-receive-pack"),
            ("POST", "git-receive-pack?service=git-receive-pack"),
            ("POST", "info/refs?service=git-receive-pack"),
            ("PUT", "git-receive-pack"),
        ] {
            assert!(
                parse_window_head(
                    &window_request(method, suffix, ""),
                    "native-container.invalid",
                    REPOSITORY
                )
                .is_err(),
                "{method} {suffix}"
            );
        }
        assert!(
            parse_window_head(
                &request("GET", "info/refs?service=git-upload-pack", ""),
                "native-container.invalid",
                REPOSITORY
            )
            .is_err()
        );
        let raw = window_request("GET", "info/refs?service=git-upload-pack", "");
        for name in [
            "",
            ".",
            "..",
            "../synthetic-demo",
            "synthetic/demo",
            "synthetic%2fdemo",
            "synthetic-demo?x",
            "synthetic-demo#x",
        ] {
            assert!(
                parse_window_head(&raw, "native-container.invalid", name).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn window_body_limits_and_content_types_are_service_specific() {
        for (service, maximum) in [
            ("git-upload-pack", MAX_REQUEST),
            ("git-receive-pack", MAX_RECEIVE_REQUEST),
        ] {
            for length in [0, maximum] {
                let headers = format!(
                    "Content-Type: application/x-{service}-request\r\nContent-Length: {length}\r\n"
                );
                let parsed = parse_window_head(
                    &window_request("POST", service, &headers),
                    "native-container.invalid",
                    REPOSITORY,
                )
                .expect("bounded request");
                assert_eq!(parsed.length, length);
            }
            let headers = format!(
                "Content-Type: application/x-{service}-request\r\nContent-Length: {}\r\n",
                maximum + 1
            );
            assert!(
                parse_window_head(
                    &window_request("POST", service, &headers),
                    "native-container.invalid",
                    REPOSITORY
                )
                .is_err()
            );
            assert!(
                parse_window_head(
                    &window_request("POST", service, "Content-Length: 0\r\n"),
                    "native-container.invalid",
                    REPOSITORY
                )
                .is_err()
            );
            let missing_length = format!("Content-Type: application/x-{service}-request\r\n");
            assert!(
                parse_window_head(
                    &window_request("POST", service, &missing_length),
                    "native-container.invalid",
                    REPOSITORY
                )
                .is_err()
            );
            let other_service = if service == "git-upload-pack" {
                "git-receive-pack"
            } else {
                "git-upload-pack"
            };
            let wrong_type = format!(
                "Content-Type: application/x-{other_service}-request\r\nContent-Length: 0\r\n"
            );
            assert!(
                parse_window_head(
                    &window_request("POST", service, &wrong_type),
                    "native-container.invalid",
                    REPOSITORY
                )
                .is_err()
            );
            let suffix = format!("info/refs?service={service}");
            assert!(
                parse_window_head(
                    &window_request("GET", &suffix, "Content-Length: 1\r\n"),
                    "native-container.invalid",
                    REPOSITORY
                )
                .is_err()
            );
        }
    }

    #[test]
    fn window_reader_preserves_exact_body_framing() {
        for service in ["git-upload-pack", "git-receive-pack"] {
            let headers =
                format!("Content-Type: application/x-{service}-request\r\nContent-Length: 4\r\n");
            let mut raw = window_request("POST", service, &headers);
            raw.extend_from_slice(b"0000");
            let (stream, mut peer) = streams();
            peer.write_all(&raw).expect("request bytes");
            peer.shutdown(Shutdown::Write).expect("request EOF");
            let parsed = read_window_request(stream, "native-container.invalid", REPOSITORY)
                .expect("framed window request");
            assert_eq!(parsed.body, b"0000");
            assert_eq!(parsed.pin, REPOSITORY);
            assert_eq!(parsed.endpoint, service);

            let (stream, mut peer) = streams();
            peer.write_all(&raw).expect("immutable request bytes");
            peer.shutdown(Shutdown::Write).expect("request EOF");
            assert!(read_request(stream, "native-container.invalid").is_err());

            let (stream, mut peer) = streams();
            peer.write_all(&raw[..raw.len() - 1])
                .expect("truncated request bytes");
            peer.shutdown(Shutdown::Write).expect("request EOF");
            assert!(read_window_request(stream, "native-container.invalid", REPOSITORY).is_err());
        }
    }

    #[test]
    fn buffered_git_response_validates_before_writing_and_closes_failed_sends() {
        let (stream, mut peer) = streams();
        peer.write_all(&window_request(
            "GET",
            "info/refs?service=git-receive-pack",
            "",
        ))
        .expect("request bytes");
        let mut request = read_window_request(stream, "native-container.invalid", REPOSITORY)
            .expect("window request");
        assert!(
            reply_git(
                &mut request,
                "application/x-git-receive-pack-advertisement\r\nInjected: yes",
                b"0000"
            )
            .is_err()
        );
        reply_git(
            &mut request,
            "application/x-git-receive-pack-advertisement",
            b"0000",
        )
        .expect("buffered advertisement");
        drop(request);
        let mut response = String::new();
        peer.read_to_string(&mut response).expect("response bytes");
        assert_eq!(
            response,
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-git-receive-pack-advertisement\r\nCache-Control: no-store\r\nContent-Length: 4\r\nConnection: close\r\n\r\n0000"
        );

        let (stream, mut peer) = streams();
        peer.write_all(&window_request(
            "GET",
            "info/refs?service=git-receive-pack",
            "",
        ))
        .expect("request bytes");
        let mut request = read_window_request(stream, "native-container.invalid", REPOSITORY)
            .expect("window request");
        request
            .stream
            .shutdown(Shutdown::Write)
            .expect("force send failure");
        assert!(
            reply_git(
                &mut request,
                "application/x-git-receive-pack-advertisement",
                b"0000"
            )
            .is_err()
        );
        assert_eq!(
            request
                .stream
                .read(&mut [0])
                .expect("failed send closed reader"),
            0
        );
        assert!(request.reply(500, b"fallback must not append").is_err());
    }

    #[test]
    fn accepts_only_exact_read_routes_and_hosts() {
        let raw = request(
            "GET",
            "info/refs?service=git-upload-pack",
            "Git-Protocol: version=2\r\n",
        );
        let parsed = parse_head(&raw, "native-container.invalid").expect("valid advertisement");
        assert_eq!(parsed.pin, PIN);
        assert_eq!(parsed.query, "service=git-upload-pack");
        assert!(parse_head(&raw, "127.0.0.1:8042").is_err());
        for suffix in [
            "info/refs",
            "info/refs?service=git-receive-pack",
            "info/refs?service=git-upload-pack&extra=1",
            "info/refs?service=git-upload-pack#fragment",
            "info%2frefs?service=git-upload-pack",
            "../info/refs?service=git-upload-pack",
        ] {
            assert!(
                parse_head(&request("GET", suffix, ""), "native-container.invalid").is_err(),
                "{suffix}"
            );
        }
        let post = request(
            "POST",
            "git-upload-pack",
            "Content-Length: 4\r\nContent-Type: application/x-git-upload-pack-request\r\n",
        );
        assert_eq!(
            parse_head(&post, "native-container.invalid")
                .expect("valid upload-pack")
                .length,
            4
        );
    }

    #[test]
    fn rejects_duplicate_security_headers_and_all_encoding() {
        for extra in [
            "Host: native-container.invalid\r\n",
            "authorization: Bearer other\r\n",
            "X-Gateway-Service-Authorization: Bearer other\r\n",
            "Content-Length: 0\r\ncontent-length: 0\r\n",
            "Transfer-Encoding: chunked\r\n",
            "Transfer-Encoding:\r\n",
            "Content-Encoding: identity\r\n",
            "Origin: https://example.invalid\r\n",
            "X-Forwarded-Proto: https\r\n",
            "Proxy-Authorization: secret\r\n",
            "X-Demo-Reader: fixture\r\n",
            "Expect: 100-continue\r\n",
            "Git-Protocol: version=9\r\n",
        ] {
            assert!(
                parse_head(
                    &request("GET", "info/refs?service=git-upload-pack", extra),
                    "native-container.invalid"
                )
                .is_err(),
                "{extra}"
            );
            assert!(
                parse_window_head(
                    &window_request("GET", "info/refs?service=git-receive-pack", extra),
                    "native-container.invalid",
                    REPOSITORY,
                )
                .is_err(),
                "{extra}"
            );
        }
    }

    #[test]
    fn rejects_ambiguous_http_framing() {
        for extra in [
            "Content-Length: +0\r\n",
            "Content-Length: 0, 0\r\n",
            "Content-Length: 1\r\n",
            "Content-Length: 999999999999999999999999999\r\n",
            " Folded: value\r\n",
        ] {
            assert!(
                parse_head(
                    &request("GET", "info/refs?service=git-upload-pack", extra),
                    "native-container.invalid"
                )
                .is_err()
            );
            assert!(
                parse_window_head(
                    &window_request("GET", "info/refs?service=git-receive-pack", extra),
                    "native-container.invalid",
                    REPOSITORY,
                )
                .is_err()
            );
        }
        let raw = request("GET", "info/refs?service=git-upload-pack", "");
        assert!(
            parse_head(
                &raw.iter()
                    .copied()
                    .filter(|b| *b != b'\r')
                    .collect::<Vec<_>>(),
                "native-container.invalid"
            )
            .is_err()
        );
        assert!(
            parse_head(
                &request(
                    "POST",
                    "git-upload-pack",
                    "Content-Type: application/x-git-upload-pack-request\r\n"
                ),
                "native-container.invalid"
            )
            .is_err()
        );
        assert!(parse_head(&request("POST", "git-upload-pack", "Content-Type: application/x-git-upload-pack-request\r\nContent-Length: 1048577\r\n"), "native-container.invalid").is_err());
        let raw = window_request("GET", "info/refs?service=git-receive-pack", "");
        assert!(
            parse_window_head(
                &raw.iter()
                    .copied()
                    .filter(|b| *b != b'\r')
                    .collect::<Vec<_>>(),
                "native-container.invalid",
                REPOSITORY
            )
            .is_err()
        );
    }

    #[test]
    fn window_requires_both_identities_and_exact_http_version() {
        let raw = String::from_utf8(window_request(
            "GET",
            "info/refs?service=git-receive-pack",
            "",
        ))
        .expect("ASCII request");
        for invalid in [
            raw.replace("Authorization: Bearer reader\r\n", ""),
            raw.replace("X-Gateway-Service-Authorization: Bearer service\r\n", ""),
            raw.replace("HTTP/1.1", "HTTP/1.0"),
            format!("\r\n{raw}"),
        ] {
            assert!(
                parse_window_head(invalid.as_bytes(), "native-container.invalid", REPOSITORY)
                    .is_err()
            );
        }
    }

    #[test]
    fn read_backend_cannot_execute_receive_pack() {
        for (method, suffix, headers) in [
            ("GET", "info/refs?service=git-receive-pack", ""),
            (
                "POST",
                "git-receive-pack",
                "Content-Type: application/x-git-receive-pack-request\r\nContent-Length: 0\r\n",
            ),
        ] {
            let (stream, mut peer) = streams();
            peer.write_all(&window_request(method, suffix, headers))
                .expect("request bytes");
            let request = read_window_request(stream, "native-container.invalid", REPOSITORY)
                .expect("window receive request");
            let result = prepare_git(&request, Path::new("/nonexistent-window-repository"));
            assert_eq!(
                result
                    .err()
                    .expect("receive rejected before repository access")
                    .to_string(),
                "read-only Git backend required"
            );
        }
        for (endpoint, content_type) in [
            ("info/refs", "application/x-git-receive-pack-advertisement"),
            ("git-receive-pack", "application/x-git-receive-pack-result"),
            ("git-receive-pack", "application/x-git-upload-pack-result"),
        ] {
            let mut output = tempfile::tempfile().expect("tempfile");
            write!(output, "Content-Type: {content_type}\r\n\r\n0000").expect("CGI data");
            assert!(parse_git_response(output, endpoint).is_err());
        }
    }

    #[test]
    fn cgi_remains_file_backed_and_has_a_validated_body_offset() {
        let mut output = tempfile::tempfile().expect("tempfile");
        let head = b"Expires: Fri, 01 Jan 1980 00:00:00 GMT\r\nContent-Type: application/x-git-upload-pack-result\r\n\r\n";
        output.write_all(head).expect("headers");
        output.write_all(b"0000").expect("body");
        let parsed = parse_git_response(output, "git-upload-pack").expect("valid CGI");
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.body_offset, head.len() as u64);
        assert_eq!(parsed.body_length, 4);
        for head in [
            "Content-Type: text/html\r\n\r\n",
            "Content-Type: application/x-git-upload-pack-result\r\nContent-Length: 99\r\n\r\n",
            "Content-Type: application/x-git-upload-pack-result\r\nContent-Type: text/plain\r\n\r\n",
            "Content-Type: application/x-git-upload-pack-result\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            let mut output = tempfile::tempfile().expect("tempfile");
            output.write_all(head.as_bytes()).expect("CGI data");
            assert!(parse_git_response(output, "git-upload-pack").is_err());
        }
    }

    #[test]
    fn bridge_routes_cannot_escape_the_configured_origin() {
        assert_eq!(
            bridge_url(
                "http://gateway-bindings.internal",
                &format!("/catalog/{PIN}")
            )
            .expect("catalog URL")
            .host_str(),
            Some("gateway-bindings.internal")
        );
        assert!(bridge_url("http://127.0.0.1:1234", "/native/synthetic-demo").is_ok());
        assert!(bridge_url("http://127.0.0.1:1234", &format!("/disclosure/{PIN}")).is_ok());
        for path in [
            "//evil.invalid/native/source",
            "/native/../secret",
            "/native/source?token=x",
            "/native/%2e%2e",
            "/catalog/not-a-pin",
            "/native/",
        ] {
            assert!(
                bridge_url("http://gateway-bindings.internal", path).is_err(),
                "{path}"
            );
        }
        assert!(
            bridge_url(
                "http://user:secret@gateway-bindings.internal",
                "/native/source"
            )
            .is_err()
        );
        assert!(bridge_url("http://gateway-bindings.internal/extra", "/native/source").is_err());
    }

    #[test]
    fn clean_environment_excludes_git_injection() {
        let mut command = Command::new("git");
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/secret")
            .env("GIT_PROTOCOL", "version=2");
        clean_environment(&mut command);
        let env: std::collections::HashMap<_, _> = command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect();
        assert!(!env.contains_key("GIT_CONFIG_COUNT"));
        assert!(!env.contains_key("GIT_ALTERNATE_OBJECT_DIRECTORIES"));
        assert_eq!(
            env.get("GIT_CONFIG_GLOBAL").map(String::as_str),
            Some("/dev/null")
        );
        assert_eq!(
            env.get("GIT_PROTOCOL").map(String::as_str),
            Some("version=2")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn subprocess_installs_all_linux_resource_limits() {
        let mut output = tempfile::tempfile().expect("tempfile");
        let mut command = Command::new("/usr/bin/cat");
        command.arg("/proc/self/limits");
        assert!(
            run_bounded(&mut command, b"", output.try_clone().expect("clone"))
                .expect("child limits")
                .success()
        );
        output.seek(SeekFrom::Start(0)).expect("rewind");
        let mut data = String::new();
        output.read_to_string(&mut data).expect("limits output");
        for (name, maximum) in [
            ("Max cpu time", 25_u64),
            ("Max address space", 1024 * 1024 * 1024),
            ("Max file size", MAX_RESPONSE),
            ("Max open files", 128),
            ("Max core file size", 0),
        ] {
            let values: Vec<_> = data
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .expect("named kernel limit")
                .split_whitespace()
                .collect();
            let soft: u64 = values[0].parse().expect("finite soft limit");
            let hard: u64 = values[1].parse().expect("finite hard limit");
            assert_eq!(soft, hard, "{name}");
            assert!(hard <= maximum, "{name}: {hard} > {maximum}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn subprocess_input_avoids_pipe_deadlock_and_deadline_kills_child() {
        let mut output = tempfile::tempfile().expect("tempfile");
        let mut command = Command::new("/usr/bin/cat");
        let input = vec![b'x'; MAX_REQUEST];
        let status = run_with_timeout(
            &mut command,
            &input,
            &mut output,
            Duration::from_secs(3),
            MAX_REQUEST,
        )
        .expect("cat completes");
        assert!(status.success());
        assert_eq!(
            output.metadata().expect("metadata").len(),
            MAX_REQUEST as u64
        );
        let mut command = Command::new("/usr/bin/sleep");
        command.arg("10");
        let start = Instant::now();
        assert!(
            run_with_timeout(
                &mut command,
                b"",
                &mut output,
                Duration::from_millis(30),
                MAX_REQUEST
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn larger_pack_input_does_not_expand_the_read_only_subprocess_limit() {
        let mut output = tempfile::tempfile().expect("tempfile");
        let mut input = vec![b'x'; MAX_PACK];
        let mut command = Command::new("/usr/bin/cat");
        assert!(
            run_bounded(
                &mut command,
                &input[..MAX_REQUEST + 1],
                output.try_clone().expect("clone")
            )
            .is_err()
        );
        assert!(
            run_bounded_pack(&mut command, &input, output.try_clone().expect("clone"))
                .expect("bounded pack input")
                .success()
        );
        assert_eq!(output.metadata().expect("metadata").len(), MAX_PACK as u64);
        output.seek(SeekFrom::Start(0)).expect("rewind");
        let mut actual = Vec::new();
        output.read_to_end(&mut actual).expect("pack output");
        assert_eq!(actual, input);
        input.push(b'x');
        assert!(run_bounded_pack(&mut command, &input, output).is_err());
    }
}

#[cfg(all(test, feature = "gateway-publication"))]
mod native_bridge_tests {
    use super::*;
    #[test]
    fn native_reply_writes_exact_raw_frame_and_rejects_bad_layout_before_status() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut request = Request {
                stream,
                method: "POST".into(),
                pin: String::new(),
                endpoint: "native".into(),
                query: String::new(),
                headers: Vec::new(),
                body: Vec::new(),
            };
            let invalid =
                crate::native_frame::Frame::json(serde_json::json!({"proof_part":"proof"}));
            assert!(reply_native_frame(&mut request, invalid).is_err());
            let frame = crate::native_frame::Frame {
                payload: serde_json::json!({"proof_part":"proof","output_part":"output"}),
                parts: std::collections::BTreeMap::from([
                    ("proof".into(), vec![0, 255, 1, 128]),
                    ("output".into(), vec![13; 64 * 1024]),
                ]),
            };
            reply_native_frame(&mut request, frame).expect("reply");
        });
        let mut client = TcpStream::connect(address).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut response = Vec::new();
        client.read_to_end(&mut response).expect("response");
        server.join().expect("server");
        let offset = header_end(&response).expect("HTTP header");
        let text = std::str::from_utf8(&response[..offset]).expect("HTTP ASCII");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.to_ascii_lowercase().contains(&format!(
            "content-type: {}\r\n",
            crate::native_frame::CONTENT_TYPE
        )));
        assert!(
            text.to_ascii_lowercase()
                .contains(&format!("content-length: {}\r\n", response.len() - offset))
        );
        let frame = crate::native_frame::Frame::decode(
            response[offset..].to_vec(),
            crate::native_frame::RESPONSE_MAX,
        )
        .expect("frame");
        assert_eq!(frame.parts.len(), 2);
        assert_eq!(frame.parts["proof"], [0, 255, 1, 128]);
        assert_eq!(frame.parts["output"], vec![13; 64 * 1024]);
    }
    fn accepts(host: &str, extra: &str, secret: &str) -> bool {
        accepts_type(host, extra, secret, crate::native_frame::CONTENT_TYPE)
    }
    fn accepts_type(host: &str, extra: &str, secret: &str, content_type: &str) -> bool {
        accepts_configured(
            host,
            extra,
            secret,
            content_type,
            "synthetic-service-value-0000000000000000",
        )
    }
    fn accepts_configured(
        host: &str,
        extra: &str,
        secret: &str,
        content_type: &str,
        configured_secret: &str,
    ) -> bool {
        accepts_digest(
            host,
            extra,
            secret,
            content_type,
            crate::policy::digest(configured_secret.as_bytes()),
        )
    }
    fn accepts_digest(
        host: &str,
        extra: &str,
        secret: &str,
        content_type: &str,
        expected: String,
    ) -> bool {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            read_native_request(s, "native-container.invalid", &expected).is_ok()
        });
        let mut client = TcpStream::connect(addr).unwrap();
        let body = crate::native_frame::Frame::json(serde_json::json!({}))
            .encoded_header(crate::native_frame::REQUEST_MAX)
            .unwrap()
            .0;
        let request = format!(
            "POST /native/v1 HTTP/1.1\r\nHost: {host}\r\nContent-Type: {content_type}\r\nAuthorization: Bearer fixture\r\nX-Gateway-Service-Authorization: Bearer {secret}\r\nContent-Length: {}\r\n{extra}\r\n",
            body.len()
        );
        let mut request = request.into_bytes();
        request.extend_from_slice(&body);
        let _ = client.write_all(&request);
        let _ = client.shutdown(Shutdown::Write);
        thread.join().unwrap()
    }
    #[test]
    fn native_service_hash_matches_the_shared_controller_golden_vector() {
        let secret = "S".repeat(64);
        let digest = "eafca4c0fae9d1fc15080c4a3b9170bfbecec255164f6328fbf663279481e808";
        assert_eq!(crate::policy::digest(secret.as_bytes()), digest);
        assert!(accepts_digest(
            "native-container.invalid",
            "",
            &secret,
            crate::native_frame::CONTENT_TYPE,
            digest.into()
        ));
        let entire_header_hash = crate::policy::digest(format!("Bearer {secret}").as_bytes());
        assert!(!accepts_digest(
            "native-container.invalid",
            "",
            &secret,
            crate::native_frame::CONTENT_TYPE,
            entire_header_hash
        ));
    }
    #[test]
    fn native_service_secret_has_the_same_exact_grammar_as_the_controller() {
        let accepts = |secret: &str| {
            accepts_configured(
                "native-container.invalid",
                "",
                secret,
                crate::native_frame::CONTENT_TYPE,
                secret,
            )
        };
        assert!(accepts(&"!".repeat(32)));
        assert!(accepts(&"~".repeat(256)));
        for secret in [
            "a".repeat(31),
            "a".repeat(257),
            format!("{} {}", "a".repeat(16), "b".repeat(16)),
            format!("{}\t{}", "a".repeat(16), "b".repeat(16)),
            format!("{}é", "a".repeat(32)),
        ] {
            assert!(!accepts(&secret));
        }
    }
    #[test]
    fn exact_internal_host_and_service_are_checked_before_body() {
        let secret = "synthetic-service-value-0000000000000000";
        assert!(accepts("native-container.invalid", "", secret));
        assert!(accepts("native-container.invalid:8080", "", secret));
        assert!(!accepts_type(
            "native-container.invalid",
            "",
            secret,
            "application/json"
        ));
        assert!(!accepts_type(
            "native-container.invalid",
            "",
            secret,
            "application/vnd.heddle.native-frame-v1; charset=utf-8"
        ));
        assert!(!accepts("native.invalid", "", secret));
        assert!(!accepts("native-container.invalid:443", "", secret));
        assert!(!accepts(
            "native-container.invalid",
            "Authorization: Bearer second\r\n",
            secret
        ));
        assert!(!accepts(
            "native-container.invalid",
            "Transfer-Encoding: chunked\r\n",
            secret
        ));
        assert!(!accepts(
            "native-container.invalid",
            "X-Forwarded-Host: attacker\r\n",
            secret
        ));
        assert!(!accepts(
            "native-container.invalid",
            "",
            "synthetic-wrong-service-value-000000000"
        ));
    }
}
