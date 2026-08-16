use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value as JsonValue, json};
use url::Url;

use super::{MAX_PORT_INPUT_BYTES, Port, PortContext, PortDescriptor, PortResult};

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// An outbound HTTP request after port policy validation.
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
    /// The host transport declined an oversized response.
    #[error("response too large")]
    TooLarge,
    /// Connectivity failed without exposing dependency diagnostics.
    #[error("request failed")]
    Transport,
}

/// Host-owned connectivity for `/ports/http`.
///
/// Custom implementations are trusted policy code. They must not follow a
/// redirect without enforcing authority equivalent to the original allowlist.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Performs one already-validated request.
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpTransportError>;
}

/// Reusable HTTP transport for the `/ports/http` port.
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
            body.extend_from_slice(&chunk);
        }

        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// URL allowlist for hosts that attenuate `/ports/http`.
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

#[derive(Clone)]
pub(super) enum HttpPolicy {
    Unrestricted,
    Allowlist(HttpAllowlist),
}

impl HttpPolicy {
    fn allows(&self, candidate: &Url) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Allowlist(allowlist) => allowlist.allows(candidate),
        }
    }
}

pub(super) struct HttpPort {
    policy: HttpPolicy,
    transport: Arc<dyn HttpTransport>,
}

impl HttpPort {
    pub(super) fn new(allowlist: HttpAllowlist, transport: impl HttpTransport + 'static) -> Self {
        Self {
            policy: HttpPolicy::Allowlist(allowlist),
            transport: Arc::new(transport),
        }
    }

    pub(super) fn unrestricted(transport: impl HttpTransport + 'static) -> Self {
        Self {
            policy: HttpPolicy::Unrestricted,
            transport: Arc::new(transport),
        }
    }
}

#[async_trait]
impl Port for HttpPort {
    fn descriptor(&self) -> PortDescriptor {
        PortDescriptor::new(
            "Make one HTTP request through the configured host transport.",
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
            "Host HTTP policy and transport apply.",
            "30 second timeout.",
            "256 KiB request body; response remains activation-local until the script reduces it.",
        ])
    }

    async fn execute(&self, _context: PortContext, arguments: JsonValue) -> PortResult {
        let method = arguments["method"].as_str().unwrap_or_default();
        if !matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD") {
            return PortResult::error("unsupported HTTP method");
        }
        let raw_url = arguments["url"].as_str().unwrap_or_default();
        let url = match Url::parse(raw_url) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => url,
            _ => return PortResult::error("invalid HTTP URL"),
        };
        // THREAT[TM-CAP-004]: The host selects unrestricted or attenuated HTTP
        // when it constructs the port registry; model input cannot widen it.
        if !self.policy.allows(&url) {
            return PortResult::error("HTTP URL is not allowed");
        }
        let headers = match parse_headers(arguments.get("headers")) {
            Ok(headers) => headers,
            Err(error) => return PortResult::error(error),
        };
        let body = arguments["body"]
            .as_str()
            .map(|body| body.as_bytes().to_vec());
        if body
            .as_ref()
            .is_some_and(|body| body.len() > MAX_PORT_INPUT_BYTES)
        {
            return PortResult::error("HTTP request body limit exceeded");
        }
        let request = HttpRequest {
            method: method.to_owned(),
            url: url.to_string(),
            headers,
            body,
        };
        let response = match tokio::time::timeout(HTTP_TIMEOUT, self.transport.send(request)).await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return PortResult::error(error.to_string()),
            Err(_) => return PortResult::error(HttpTransportError::Timeout.to_string()),
        };
        let response_header_bytes = response
            .headers
            .iter()
            .try_fold(0usize, |size, (name, value)| {
                size.checked_add(name.len() + value.len())
            });
        if response.headers.len() > 64 || response_header_bytes.is_none_or(|size| size > 64 * 1024)
        {
            return PortResult::error(HttpTransportError::TooLarge.to_string());
        }
        let body = String::from_utf8_lossy(&response.body);
        PortResult::success(
            json!({"status": response.status, "headers": response.headers, "body": body}),
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

    #[test]
    fn unrestricted_policy_accepts_any_http_origin_tm_cap_004() {
        let policy = HttpPolicy::Unrestricted;

        assert!(policy.allows(&Url::parse("https://example.com/research").unwrap()));
        assert!(policy.allows(&Url::parse("http://127.0.0.1:8080/status").unwrap()));
    }

    fn serve_once(response: impl Into<Vec<u8>>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response = response.into();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).unwrap() > 0);
            stream.write_all(&response).unwrap();
        });
        (format!("http://{address}/"), server)
    }

    fn request(url: String) -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            url,
            headers: BTreeMap::new(),
            body: None,
        }
    }

    #[tokio::test]
    async fn reqwest_transport_rejects_redirect_escape_tm_cap_004() {
        let (url, server) = serve_once(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/escaped\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );

        let response = ReqwestHttpTransport::new()
            .unwrap()
            .send(request(url))
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status, 302);
    }

    #[tokio::test]
    async fn reqwest_transport_accepts_response_larger_than_a_process_value() {
        let body = "x".repeat(2 * 1024 * 1024);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let (url, server) = serve_once(response);

        let response = ReqwestHttpTransport::new()
            .unwrap()
            .send(request(url))
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.body.len(), 2 * 1024 * 1024);
    }
}
