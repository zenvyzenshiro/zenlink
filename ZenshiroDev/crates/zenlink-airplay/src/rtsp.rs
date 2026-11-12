//! RTSP framing as AirPlay uses it: a header block ending in a blank line, then
//! `Content-Length` bytes of body; replies carry the request's CSeq.

use std::collections::HashMap;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

pub struct Request {
    pub method: String,
    pub url: String,
    /// Lower-case names.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// Biggest header block or body accepted (iOS sends a few kB at most).
const LIMIT: usize = 1 << 20;

/// The next request; `None` at the end of the connection or on garbage.
pub async fn read<R: AsyncBufRead + Unpin>(r: &mut R) -> Option<Request> {
    let mut head = Vec::new();
    loop {
        // Bounded read: a peer that never sends a newline must not grow the buffer forever.
        let left = (LIMIT + 1).saturating_sub(head.len()) as u64;
        let n = (&mut *r)
            .take(left)
            .read_until(b'\n', &mut head)
            .await
            .ok()?;
        if n == 0 || head.len() > LIMIT {
            return None;
        }
        if head == b"\r\n" {
            head.clear(); // a stray blank line between requests
        } else if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let url = first.next().unwrap_or("").to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len > LIMIT {
        return None;
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await.ok()?;
    Some(Request {
        method,
        url,
        headers,
        body,
    })
}

pub fn reply(status: &str, cseq: &str, extra: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut s = format!(
        "RTSP/1.0 {status}\r\nCSeq: {cseq}\r\nServer: AirTunes/{}\r\n",
        crate::info::SOURCE_VERSION
    );
    for (k, v) in extra {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut out = s.into_bytes();
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn reads_requests_back_to_back() {
        let raw = b"POST /pair-verify RTSP/1.0\r\nCSeq: 3\r\nContent-Length: 4\r\nX-Apple: a:b\r\n\r\nabcdGET /info RTSP/1.0\r\nCSeq: 4\r\n\r\n";
        let mut r = tokio::io::BufReader::new(&raw[..]);
        let a = super::read(&mut r).await.unwrap();
        assert_eq!(
            (a.method.as_str(), a.url.as_str(), &a.body[..]),
            ("POST", "/pair-verify", &b"abcd"[..])
        );
        assert_eq!(a.headers["x-apple"], "a:b");
        let b = super::read(&mut r).await.unwrap();
        assert_eq!(
            (b.method.as_str(), b.headers["cseq"].as_str()),
            ("GET", "4")
        );
        assert!(super::read(&mut r).await.is_none());
        let out = super::reply("200 OK", "4", &[("Content-Type", "x".into())], b"hi");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "RTSP/1.0 200 OK\r\nCSeq: 4\r\nServer: AirTunes/220.68\r\nContent-Type: x\r\nContent-Length: 2\r\n\r\nhi"
        );
    }
}
