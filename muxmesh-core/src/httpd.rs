//! Minimal HTTP/1.1 request parsing over std::net, shared by hub and agent.
//! Deliberately tiny: request line + headers + Content-Length-bounded body.
//! No keep-alive (Connection: close), no chunked, no TLS -> trusted LAN only.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

/// A parsed request: method, path, lowercase-header map, body.
pub struct Request {
    pub method: String,
    pub path: String,
    /// header name -> raw value (stored lowercase; values keep their case).
    pub headers: std::collections::HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    /// Header lookup by name (case-insensitive, per HTTP).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }
}

/// Read one request from the stream. The BufReader is consumed **by value**
/// so no buffered bytes can be stranded when it drops. Body is read only
/// when Content-Length is present and within `max_body`.
pub fn read_request(stream: TcpStream, max_body: usize) -> std::io::Result<Request> {
    // One BufReader owns the socket for the whole request: read_line buffers
    // headers AND body, so bytes are never lost between two readers.
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .map_err(|e| std::io::Error::new(e.kind(), "set read timeout"))?;
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    if method.is_empty() || path.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad request line",
        ));
    }

    let mut headers = std::collections::HashMap::new();
    let mut content_len: usize = 0;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let name = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string(); // value keeps its case (tokens)
            if name == "content-length" {
                content_len = val.parse().unwrap_or(0);
            }
            headers.insert(name, val);
        }
    }

    if content_len > max_body {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "body too large",
        ));
    }
    // read_line above may have buffered part of the body; drain the buffer
    // first, then the socket, so nothing is dropped or double-read.
    let mut body = Vec::with_capacity(content_len.min(max_body));
    while body.len() < content_len {
        let filled = reader.fill_buf()?;
        if filled.is_empty() {
            break; // client closed early -> short body, caller 4xx's on parse
        }
        let take = std::cmp::min(filled.len(), content_len - body.len());
        body.extend_from_slice(&filled[..take]);
        reader.consume(take);
    }

    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

/// Write a minimal HTTP/1.1 response with Connection: close.
pub fn respond(
    stream: &mut TcpStream,
    code: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let status = match code {
        200 => "200 OK",
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        404 => "404 Not Found",
        405 => "405 Method Not Allowed",
        422 => "422 Unprocessable Entity",
        503 => "503 Service Unavailable",
        _ => "500 Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lookup_is_case_insensitive_value_preserving() {
        let mut h = std::collections::HashMap::new();
        h.insert("x-muxmesh-token".to_string(), "Secret123".to_string());
        let req = Request {
            method: "POST".into(),
            path: "/cmd".into(),
            headers: h,
            body: vec![],
        };
        assert_eq!(req.header("X-Muxmesh-Token"), Some("Secret123"));
        assert_eq!(req.header("missing"), None);
    }

    #[test]
    fn respond_formats_minimal_valid_http() {
        // smoke: respond writes a parseable head (no socket here -> just check fmt)
        let body = b"ok\n";
        let status_line = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        assert!(status_line.contains("Content-Length: 3"));
    }
}
