use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value as JsonValue, json};
use url::Url;

use super::{
    Builtin, BuiltinContext, BuiltinManual, BuiltinResult, MAX_TOOL_INPUT_BYTES,
    MAX_TOOL_OUTPUT_BYTES,
};

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// An outbound HTTP request after built-in policy validation.
///
/// This type intentionally does not implement `Debug`: headers may contain
/// credentials supplied by the model or embedding host.
#[derive(Clone)]
pub struct HttpRequest {
    /// HTTP method.
    pub method: String,
    /// Absolute, allowlisted HTTP(S) URL.
    pub url: String,
    /// Request headers.
    pub headers: BTreeMap<String, String>,
    /// Optional request body.
    pub body: Option<Vec<u8>>,
    /// Maximum accepted response body size.
    pub max_response_bytes: usize,
}

/// A response returned by a host-owned HTTP transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: BTreeMap<String, String>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// Typed failure returned by a host-owned HTTP transport.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HttpTransportError {
    /// The host denied the request.
    #[error("request denied")]
    Denied,
    /// The request timed out.
    #[error("request timed out")]
    Timeout,
    /// The response exceeded its configured bound.
    #[error("response too large")]
    TooLarge,
    /// Connectivity failed without exposing dependency diagnostics.
    #[error("request failed")]
    Transport,
}

/// Host-owned connectivity for `/bin/http`.
///
/// Custom implementations are trusted policy code. They must not follow a
/// redirect without enforcing authority equivalent to the original allowlist.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Performs one already-validated request.
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpTransportError>;
}

/// Reusable bounded HTTP transport for the `/bin/http` built-in.
///
/// Redirects are returned to the caller rather than followed so a validated
/// URL cannot redirect outside its host-selected allowlist.
#[derive(Clone)]
pub struct ReqwestHttpTransport {
    client: reqwest::Client,
}

impl ReqwestHttpTransport {
    /// Builds the standard Svit HTTP transport.
    pub fn new() -> Result<Self, HttpTransportError> {
        let client = reqwest::Client::builder()
            // THREAT[TM-CAP-004]: The allowlist validates the requested URL;
            // following a redirect could otherwise escape that grant.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| HttpTransportError::Transport)?;
        Ok(Self { client })
    }
}

#[async_trait]
impl HttpTransport for ReqwestHttpTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpTransportError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| HttpTransportError::Transport)?;
        let mut outgoing = self.client.request(method, &request.url);
        for (name, value) in request.headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| HttpTransportError::Transport)?;
            let value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| HttpTransportError::Transport)?;
            outgoing = outgoing.header(name, value);
        }
        if let Some(body) = request.body {
            outgoing = outgoing.body(body);
        }

        let mut incoming = outgoing
            .send()
            .await
            .map_err(|_| HttpTransportError::Transport)?;
        let status = incoming.status().as_u16();
        let headers = incoming
            .headers()
            .iter()
            .map(|(name, value)| {
                value
                    .to_str()
                    .map(|value| (name.to_string(), value.to_owned()))
                    .map_err(|_| HttpTransportError::Transport)
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let mut body = Vec::new();
        while let Some(chunk) = incoming
            .chunk()
            .await
            .map_err(|_| HttpTransportError::Transport)?
        {
            if body
                .len()
                .checked_add(chunk.len())
                .is_none_or(|size| size > request.max_response_bytes)
            {
                return Err(HttpTransportError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }

        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// Default-deny URL policy for `/bin/http`.
#[derive(Clone, Default)]
pub struct HttpAllowlist {
    roots: Vec<Url>,
}

impl HttpAllowlist {
    /// Creates an empty allowlist.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allows one HTTP(S) endpoint and its path descendants.
    ///
    /// Invalid URLs are ignored, so malformed host configuration fails closed.
    pub fn allow(mut self, root: impl AsRef<str>) -> Self {
        if let Ok(url) = Url::parse(root.as_ref())
            && matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
        {
            self.roots.push(url);
        }
        self
    }

    fn allows(&self, candidate: &Url) -> bool {
        self.roots.iter().any(|root| {
            root.scheme() == candidate.scheme()
                && root.host_str() == candidate.host_str()
                && root.port_or_known_default() == candidate.port_or_known_default()
                && path_is_within(root.path(), candidate.path())
        })
    }
}

pub(super) struct HttpBuiltin {
    allowlist: HttpAllowlist,
    transport: Arc<dyn HttpTransport>,
}

impl HttpBuiltin {
    pub(super) fn new(allowlist: HttpAllowlist, transport: impl HttpTransport + 'static) -> Self {
        Self {
            allowlist,
            transport: Arc::new(transport),
        }
    }
}

#[async_trait]
impl Builtin for HttpBuiltin {
    fn manual(&self) -> BuiltinManual {
        BuiltinManual::new(
            "Make one host-allowlisted HTTP request through the configured transport.",
            json!({
                "type": "object",
                "properties": {
                    "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"]},
                    "url": {"type": "string"},
                    "headers": {"type": "object", "additionalProperties": {"type": "string"}},
                    "body": {"type": "string"}
                },
                "required": ["method", "url"]
            }),
        )
        .effect("external")
        .output("JSON object containing status, response headers, and UTF-8-lossy body text.")
        .limits([
            "Host URL allowlist and transport apply.",
            "30 second timeout.",
            "256 KiB request and response bodies.",
        ])
    }

    async fn execute(&self, _context: BuiltinContext, arguments: JsonValue) -> BuiltinResult {
        let method = arguments["method"].as_str().unwrap_or_default();
        if !matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD") {
            return BuiltinResult::error("unsupported HTTP method");
        }
        let raw_url = arguments["url"].as_str().unwrap_or_default();
        let url = match Url::parse(raw_url) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => url,
            _ => return BuiltinResult::error("invalid HTTP URL"),
        };
        // THREAT[TM-CAP-004]: A model URL grants no authority. The host
        // allowlist is checked before the host transport runs.
        if !self.allowlist.allows(&url) {
            return BuiltinResult::error("HTTP URL is not allowed");
        }
        let headers = match parse_headers(arguments.get("headers")) {
            Ok(headers) => headers,
            Err(error) => return BuiltinResult::error(error),
        };
        let body = arguments["body"]
            .as_str()
            .map(|body| body.as_bytes().to_vec());
        if body
            .as_ref()
            .is_some_and(|body| body.len() > MAX_TOOL_INPUT_BYTES)
        {
            return BuiltinResult::error("HTTP request body limit exceeded");
        }
        let request = HttpRequest {
            method: method.to_owned(),
            url: url.to_string(),
            headers,
            body,
            max_response_bytes: MAX_TOOL_OUTPUT_BYTES,
        };
        let response = match tokio::time::timeout(HTTP_TIMEOUT, self.transport.send(request)).await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return BuiltinResult::error(error.to_string()),
            Err(_) => return BuiltinResult::error(HttpTransportError::Timeout.to_string()),
        };
        if response.body.len() > MAX_TOOL_OUTPUT_BYTES {
            return BuiltinResult::error(HttpTransportError::TooLarge.to_string());
        }
        let response_header_bytes = response
            .headers
            .iter()
            .try_fold(0usize, |size, (name, value)| {
                size.checked_add(name.len() + value.len())
            });
        if response.headers.len() > 64 || response_header_bytes.is_none_or(|size| size > 64 * 1024)
        {
            return BuiltinResult::error(HttpTransportError::TooLarge.to_string());
        }
        let body = String::from_utf8_lossy(&response.body);
        BuiltinResult::text(
            json!({"status": response.status, "headers": response.headers, "body": body})
                .to_string(),
        )
    }
}

fn parse_headers(value: Option<&JsonValue>) -> Result<BTreeMap<String, String>, &'static str> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let Some(object) = value.as_object() else {
        return Err("HTTP headers must be an object");
    };
    if object.len() > 64 {
        return Err("HTTP header count limit exceeded");
    }
    let mut headers = BTreeMap::new();
    let mut total_bytes = 0usize;
    for (name, value) in object {
        let Some(value) = value.as_str() else {
            return Err("HTTP header values must be text");
        };
        if name.len() + value.len() > 8 * 1024
            || name.contains(['\r', '\n'])
            || value.contains(['\r', '\n'])
        {
            return Err("invalid HTTP header");
        }
        total_bytes = total_bytes
            .checked_add(name.len() + value.len())
            .ok_or("HTTP header size limit exceeded")?;
        if total_bytes > 64 * 1024 {
            return Err("HTTP header size limit exceeded");
        }
        headers.insert(name.clone(), value.to_owned());
    }
    Ok(headers)
}

fn path_is_within(root: &str, candidate: &str) -> bool {
    root == "/"
        || candidate == root
        || candidate
            .strip_prefix(root.trim_end_matches('/'))
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    use super::*;

    fn serve_once(response: &'static [u8]) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).unwrap() > 0);
            stream.write_all(response).unwrap();
        });
        (format!("http://{address}/"), server)
    }

    fn request(url: String, max_response_bytes: usize) -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            url,
            headers: BTreeMap::new(),
            body: None,
            max_response_bytes,
        }
    }

    #[tokio::test]
    async fn reqwest_transport_rejects_redirect_escape_tm_cap_004() {
        let (url, server) = serve_once(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/escaped\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );

        let response = ReqwestHttpTransport::new()
            .unwrap()
            .send(request(url, 1024))
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status, 302);
    }

    #[tokio::test]
    async fn reqwest_transport_rejects_oversized_streamed_response() {
        let (url, server) =
            serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef");

        let error = ReqwestHttpTransport::new()
            .unwrap()
            .send(request(url, 5))
            .await
            .unwrap_err();
        server.join().unwrap();

        assert_eq!(error, HttpTransportError::TooLarge);
    }
}
