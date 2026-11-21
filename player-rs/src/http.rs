//! Minimal blocking HTTP/1.1 client over `std::net` (plain http, which is what
//! the LAN backend uses). No TLS, no external crates: request/response with
//! Content-Length or chunked bodies, plus streaming downloads to disk.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    /// Path including query string, always starting with '/'.
    pub path: String,
}

pub fn parse_url(raw: &str) -> Result<Url, String> {
    let rest = raw
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// URLs are supported, got '{raw}'"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err("URL has no host".into());
    }
    let (host, port) = match authority.rfind(':') {
        Some(i) => {
            let h = &authority[..i];
            let p: u16 = authority[i + 1..]
                .parse()
                .map_err(|_| format!("invalid port in '{raw}'"))?;
            (h.to_string(), p)
        }
        None => (authority.to_string(), 80),
    };
    Ok(Url {
        host,
        port,
        path: path.to_string(),
    })
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn connect(url: &Url, timeout: Duration) -> io::Result<TcpStream> {
    let addr = (url.host.as_str(), url.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address resolved"))?;
    let stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

type Headers = Vec<(String, String)>;

fn read_headers(stream: &mut TcpStream) -> io::Result<(u16, Headers, Vec<u8>)> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let mut lines = head.split("\r\n");
            let status_line = lines.next().unwrap_or("");
            let status = status_line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad status line"))?;
            let mut headers = Vec::new();
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            // consume the body bytes we already read past the header end
            let rest = buf.split_off(pos + 4);
            return Ok((status, headers, rest));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF in headers",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "headers too large",
            ));
        }
    }
}

// read_headers returns headers + already-read body bytes; wrap into a reader
struct PrefixedReader {
    prefix: io::Cursor<Vec<u8>>,
    stream: TcpStream,
}

impl Read for PrefixedReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match self.prefix.read(out) {
            Ok(0) => self.stream.read(out),
            other => other,
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn decode_chunked(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let mut pos = 0usize;
    loop {
        // read a size line
        let line_end = loop {
            if let Some(i) = buf[pos..].iter().position(|&b| b == b'\n') {
                break pos + i;
            }
            let n = reader.read(&mut tmp)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "chunked EOF"));
            }
            buf.extend_from_slice(&tmp[..n]);
        };
        let size_str = String::from_utf8_lossy(&buf[pos..line_end]).to_string();
        let size_str = size_str.trim_end_matches('\r');
        let size_str = size_str.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        pos = line_end + 1;
        if size == 0 {
            break;
        }
        while buf.len() - pos < size + 2 {
            let n = reader.read(&mut tmp)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "chunked EOF"));
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        out.extend_from_slice(&buf[pos..pos + size]);
        pos += size + 2; // skip data + CRLF
        buf.drain(..pos);
        pos = 0;
    }
    Ok(out)
}

/// Performs one HTTP request. `body` is `(content_type, bytes)` for POST.
pub fn request(
    method: &str,
    raw_url: &str,
    body: Option<(&str, &[u8])>,
    timeout: Duration,
) -> Result<Response, String> {
    let url = parse_url(raw_url)?;
    let mut stream = connect(&url, timeout).map_err(|e| format!("connect {}: {e}", url.host))?;
    let mut req = format!(
        "{method} {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nAccept: */*\r\n",
        url.path, url.host, url.port
    );
    if let Some((ct, data)) = body {
        req.push_str(&format!(
            "Content-Type: {ct}\r\nContent-Length: {}\r\n",
            data.len()
        ));
        req.push_str("\r\n");
        stream
            .write_all(req.as_bytes())
            .and_then(|_| stream.write_all(data))
            .map_err(|e| format!("send: {e}"))?;
    } else {
        req.push_str("\r\n");
        stream
            .write_all(req.as_bytes())
            .map_err(|e| format!("send: {e}"))?;
    }
    stream.flush().map_err(|e| format!("flush: {e}"))?;

    let (status, headers, rest) =
        read_headers(&mut stream).map_err(|e| format!("read headers: {e}"))?;
    let mut reader = PrefixedReader {
        prefix: io::Cursor::new(rest),
        stream,
    };
    let body = if header(&headers, "transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        decode_chunked(&mut reader).map_err(|e| format!("chunked body: {e}"))?
    } else if let Some(cl) =
        header(&headers, "content-length").and_then(|v| v.parse::<usize>().ok())
    {
        let mut body = Vec::with_capacity(cl.min(16 * 1024 * 1024));
        reader
            .take(cl as u64)
            .read_to_end(&mut body)
            .map_err(|e| format!("body: {e}"))?;
        body
    } else {
        let mut body = Vec::new();
        reader
            .read_to_end(&mut body)
            .map_err(|e| format!("body: {e}"))?;
        body
    };
    Ok(Response {
        status,
        headers,
        body,
    })
}

pub fn get(raw_url: &str, timeout: Duration) -> Result<Response, String> {
    request("GET", raw_url, None, timeout)
}

pub fn post_json(raw_url: &str, json: &str, timeout: Duration) -> Result<Response, String> {
    request(
        "POST",
        raw_url,
        Some(("application/json", json.as_bytes())),
        timeout,
    )
}

/// Streams a GET response body to `path` (media downloads can be large).
/// Returns the number of bytes written.
pub fn download(raw_url: &str, path: &Path, timeout: Duration) -> Result<u64, String> {
    let url = parse_url(raw_url)?;
    let mut stream = connect(&url, timeout).map_err(|e| format!("connect {}: {e}", url.host))?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nAccept: */*\r\n\r\n",
        url.path, url.host, url.port
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("send: {e}"))?;
    let (status, headers, rest) =
        read_headers(&mut stream).map_err(|e| format!("read headers: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status} for {raw_url}"));
    }
    let mut file = std::fs::File::create(path).map_err(|e| format!("create {path:?}: {e}"))?;
    let mut reader = PrefixedReader {
        prefix: io::Cursor::new(rest),
        stream,
    };
    let total = if header(&headers, "transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        let mut total = 0u64;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 65536];
        let mut pos = 0usize;
        loop {
            let line_end = loop {
                if let Some(i) = buf[pos..].iter().position(|&b| b == b'\n') {
                    break pos + i;
                }
                let n = reader.read(&mut tmp).map_err(|e| format!("body: {e}"))?;
                if n == 0 {
                    return Err("chunked EOF".into());
                }
                buf.extend_from_slice(&tmp[..n]);
            };
            let size_str = String::from_utf8_lossy(&buf[pos..line_end]).to_string();
            let size = usize::from_str_radix(
                size_str
                    .trim_end_matches('\r')
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim(),
                16,
            )
            .map_err(|_| "bad chunk size".to_string())?;
            pos = line_end + 1;
            if size == 0 {
                break;
            }
            while buf.len() - pos < size + 2 {
                let n = reader.read(&mut tmp).map_err(|e| format!("body: {e}"))?;
                if n == 0 {
                    return Err("chunked EOF".into());
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            file.write_all(&buf[pos..pos + size])
                .map_err(|e| format!("write: {e}"))?;
            total += size as u64;
            pos += size + 2;
            buf.drain(..pos);
            pos = 0;
        }
        total
    } else {
        let mut total = 0u64;
        let mut tmp = [0u8; 65536];
        loop {
            let n = reader.read(&mut tmp).map_err(|e| format!("body: {e}"))?;
            if n == 0 {
                break;
            }
            file.write_all(&tmp[..n])
                .map_err(|e| format!("write: {e}"))?;
            total += n as u64;
        }
        total
    };
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::thread;

    fn serve_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            // read request line + headers
            let mut content_length = 0usize;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line.to_lowercase().starts_with("content-length:") {
                    content_length = line.split(':').nth(1).unwrap().trim().parse().unwrap();
                }
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            let mut body = vec![0u8; content_length];
            std::io::Read::read_exact(&mut reader, &mut body).ok();
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        });
        port
    }

    #[test]
    fn parse_url_works() {
        let u = parse_url("http://127.0.0.1:5000/api/display/ABCD?x=1").unwrap();
        assert_eq!(u.host, "127.0.0.1");
        assert_eq!(u.port, 5000);
        assert_eq!(u.path, "/api/display/ABCD?x=1");
        let u = parse_url("http://example.com").unwrap();
        assert_eq!(u.port, 80);
        assert_eq!(u.path, "/");
        assert!(parse_url("https://x").is_err());
        assert!(parse_url("http://").is_err());
        assert!(parse_url("http://h:bad/").is_err());
    }

    #[test]
    fn get_parses_response() {
        let port = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}",
        );
        let resp = get(
            &format!("http://127.0.0.1:{port}/api/state"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("content-type"), Some("application/json"));
        assert_eq!(resp.body, b"{\"ok\":true}");
    }

    #[test]
    fn post_sends_body_and_reads_status() {
        let port = serve_once("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        let resp = post_json(
            &format!("http://127.0.0.1:{port}/api/display/XXXX"),
            "{}",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body.is_empty());
    }

    #[test]
    fn chunked_response_is_decoded() {
        let port = serve_once(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        );
        let resp = get(&format!("http://127.0.0.1:{port}/"), Duration::from_secs(5)).unwrap();
        assert_eq!(resp.body, b"hello world");
    }

    #[test]
    fn download_streams_to_file() {
        let payload = vec![7u8; 100_000];
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        resp.extend_from_slice(&payload);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).map(|_| ());
            stream.write_all(&resp).unwrap();
        });
        let dir = std::env::temp_dir().join(format!("fossignage-http-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dl.bin");
        let n = download(
            &format!("http://127.0.0.1:{port}/static/media/x.png"),
            &path,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(n, 100_000);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 100_000);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn connection_refused_is_error() {
        // port 1 is closed on loopback in practice
        let r = get("http://127.0.0.1:1/", Duration::from_secs(2));
        assert!(r.is_err());
    }
}
