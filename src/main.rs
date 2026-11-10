use std::collections::HashMap;
use std::convert::Infallible;
use std::env;
use std::fmt;
use std::fs;
use std::net::SocketAddr;
use std::process;
use std::sync::Arc;

use bytes::Bytes;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use tokio::net::TcpListener;

#[derive(Debug, Deserialize, Default)]
struct Config {
    #[serde(default, alias = "fullurl", alias = "full_url")]
    #[serde(rename = "fullUrl")]
    full_url: HashMap<String, ResponseRule>,
    #[serde(default, alias = "hosts", alias = "domain", alias = "domains")]
    host: HashMap<String, ResponseRule>,
    #[serde(default, alias = "paths", alias = "uri", alias = "uris")]
    path: HashMap<String, ResponseRule>,
    #[serde(default, alias = "fallback")]
    default: Option<ResponseRule>,
}

#[derive(Debug, Deserialize, Clone)]
struct ResponseRule {
    #[serde(alias = "statusCode", alias = "status_code")]
    status: Option<u16>,
    #[serde(default)]
    headers: RawHeaders,
    #[serde(alias = "body")]
    content: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct RawHeaders(Vec<(String, String)>);

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HeaderValueType {
    String(String),
    Int(i64),
    Uint(u64),
    Float(f64),
    Bool(bool),
    List(Vec<String>),
}

impl<'de> Deserialize<'de> for RawHeaders {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HeadersVisitor;

        impl<'de> Visitor<'de> for HeadersVisitor {
            type Value = RawHeaders;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a dictionary/map of headers (e.g. key: value)")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(RawHeaders::default())
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(RawHeaders::default())
            }

            fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut pairs = Vec::new();
                while let Some((k, v)) = access.next_entry::<String, HeaderValueType>()? {
                    match v {
                        HeaderValueType::String(s) => pairs.push((k, s)),
                        HeaderValueType::Int(i) => pairs.push((k, i.to_string())),
                        HeaderValueType::Uint(u) => pairs.push((k, u.to_string())),
                        HeaderValueType::Float(f) => pairs.push((k, f.to_string())),
                        HeaderValueType::Bool(b) => pairs.push((k, b.to_string())),
                        HeaderValueType::List(items) => {
                            for item in items {
                                pairs.push((k.clone(), item));
                            }
                        }
                    }
                }
                Ok(RawHeaders(pairs))
            }
        }

        deserializer.deserialize_map(HeadersVisitor)
    }
}

#[derive(Clone, Debug)]
pub struct CannedResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl CannedResponse {
    fn from_rule(rule: &ResponseRule) -> Result<Self, String> {
        let status_code = rule.status.unwrap_or(200);
        let status = StatusCode::from_u16(status_code)
            .map_err(|e| format!("invalid status code {status_code}: {e}"))?;

        let mut headers = HeaderMap::with_capacity(rule.headers.0.len());
        for (k, v) in &rule.headers.0 {
            let name = HeaderName::from_bytes(k.trim().as_bytes())
                .map_err(|e| format!("invalid header name '{k}': {e}"))?;
            let val = HeaderValue::from_str(v)
                .map_err(|e| format!("invalid header value '{v}' for '{k}': {e}"))?;
            headers.append(name, val);
        }

        let body = Bytes::from(rule.content.clone().unwrap_or_default());

        Ok(Self {
            status,
            headers,
            body,
        })
    }

    pub fn to_http_response(&self) -> Response<Full<Bytes>> {
        let mut resp = Response::new(Full::new(self.body.clone()));
        *resp.status_mut() = self.status;
        *resp.headers_mut() = self.headers.clone();
        resp
    }
}

#[derive(Debug)]
pub struct Router {
    full_urls: HashMap<String, CannedResponse>,
    hosts: HashMap<String, CannedResponse>,
    paths: HashMap<String, CannedResponse>,
    default_response: CannedResponse,
}

impl Router {
    pub fn from_yaml(yaml: &str) -> Result<Self, String> {
        let config: Config =
            serde_yaml::from_str(yaml).map_err(|e| format!("failed to parse YAML config: {e}"))?;

        let mut full_urls = HashMap::new();
        let mut hosts = HashMap::new();
        let mut paths = HashMap::new();

        for (url, rule) in &config.full_url {
            let response = CannedResponse::from_rule(rule)?;
            let (with_scheme, without_scheme) = normalize_full_url(url);
            if let Some(ws) = with_scheme {
                full_urls.entry(ws).or_insert_with(|| response.clone());
            }
            full_urls.entry(without_scheme).or_insert(response);
        }

        for (host, rule) in &config.host {
            let response = CannedResponse::from_rule(rule)?;
            let norm_host = normalize_host(host);
            hosts.entry(norm_host).or_insert(response);
        }

        for (path, rule) in &config.path {
            let response = CannedResponse::from_rule(rule)?;
            let norm_path = normalize_path(path);
            paths.entry(norm_path).or_insert(response);
        }

        let default_response = match &config.default {
            Some(rule) => CannedResponse::from_rule(rule)?,
            None => CannedResponse {
                status: StatusCode::NOT_FOUND,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        };

        Ok(Self {
            full_urls,
            hosts,
            paths,
            default_response,
        })
    }

    pub fn resolve(
        &self,
        scheme: &str,
        host_header: Option<&str>,
        path: &str,
        path_and_query: &str,
    ) -> &CannedResponse {
        let host_lower = host_header.map(|h| h.trim().to_ascii_lowercase());
        let host_no_port = host_lower
            .as_deref()
            .and_then(|h| strip_port(h))
            .map(str::to_string);

        // 1. Check fullurl (first matching fullurl wins)
        if !self.full_urls.is_empty() {
            if let Some(ref h) = host_lower
                && let Some(resp) = self.lookup_full_url(scheme, h, path, path_and_query) {
                    return resp;
                }
            if let Some(ref h_np) = host_no_port
                && let Some(resp) = self.lookup_full_url(scheme, h_np, path, path_and_query) {
                    return resp;
                }
        }

        // 2. Check host
        if !self.hosts.is_empty() {
            if let Some(ref h) = host_lower
                && let Some(resp) = self.hosts.get(h.as_str()) {
                    return resp;
                }
            if let Some(ref h_np) = host_no_port
                && let Some(resp) = self.hosts.get(h_np.as_str()) {
                    return resp;
                }
        }

        // 3. Check path
        if !self.paths.is_empty() {
            if path_and_query != path
                && let Some(resp) = self.paths.get(path_and_query) {
                    return resp;
                }
            if let Some(resp) = self.paths.get(path) {
                return resp;
            }
        }

        // 4. Default (none)
        &self.default_response
    }

    fn lookup_full_url(
        &self,
        scheme: &str,
        host: &str,
        path: &str,
        path_and_query: &str,
    ) -> Option<&CannedResponse> {
        if path_and_query != path {
            let key_scheme_pq = format!("{scheme}://{host}{path_and_query}");
            if let Some(resp) = self.full_urls.get(&key_scheme_pq) {
                return Some(resp);
            }
        }

        let key_scheme_p = format!("{scheme}://{host}{path}");
        if let Some(resp) = self.full_urls.get(&key_scheme_p) {
            return Some(resp);
        }

        if path_and_query != path {
            let key_pq = format!("{host}{path_and_query}");
            if let Some(resp) = self.full_urls.get(&key_pq) {
                return Some(resp);
            }
        }

        let key_p = format!("{host}{path}");
        if let Some(resp) = self.full_urls.get(&key_p) {
            return Some(resp);
        }

        None
    }
}

fn strip_port(host: &str) -> Option<&str> {
    if host.starts_with('[') {
        // IPv6 literal like [::1]:7890
        let end_bracket = host.find(']')?;
        let rest = &host[end_bracket + 1..];
        if rest.starts_with(':') {
            Some(&host[..=end_bracket])
        } else {
            None
        }
    } else {
        host.rsplit_once(':').map(|(h, _)| h)
    }
}

fn normalize_host(raw: &str) -> String {
    let s = raw.trim();
    let s = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
        .or_else(|| s.strip_prefix("HTTP://"))
        .or_else(|| s.strip_prefix("HTTPS://"))
        .unwrap_or(s);
    s.trim_end_matches('/').to_ascii_lowercase()
}

fn normalize_path(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() || s == "/" {
        "/".to_string()
    } else if s.starts_with('/') {
        s.to_string()
    } else {
        format!("/{s}")
    }
}

fn normalize_full_url(raw: &str) -> (Option<String>, String) {
    let s = raw.trim();
    let lower = s.to_ascii_lowercase();
    let (scheme, rest) = if lower.starts_with("http://") {
        (Some("http"), &s[7..])
    } else if lower.starts_with("https://") {
        (Some("https"), &s[8..])
    } else {
        (None, s)
    };

    let (host_part, path_part) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };

    let norm_host = host_part.to_ascii_lowercase();
    let without_scheme = format!("{norm_host}{path_part}");
    let with_scheme = scheme.map(|sc| format!("{sc}://{without_scheme}"));

    (with_scheme, without_scheme)
}

fn handle_request(req: Request<Incoming>, router: &Router) -> Response<Full<Bytes>> {
    let scheme = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().scheme_str())
        .unwrap_or("http");

    let host_header = req
        .headers()
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()));

    let path = req.uri().path();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(path);

    router
        .resolve(scheme, host_header, path, path_and_query)
        .to_http_response()
}

fn get_config_path() -> String {
    for var in ["CONFIG", "CANNED_CONFIG"] {
        if let Ok(val) = env::var(var)
            && !val.trim().is_empty() {
                return val;
            }
    }
    if let Some(arg) = env::args().nth(1) {
        return arg;
    }
    "canned.yaml".to_string()
}

fn get_port() -> u16 {
    for var in ["PORT", "CANNED_PORT"] {
        if let Ok(val) = env::var(var)
            && let Ok(port) = val.trim().parse::<u16>() {
                return port;
            }
    }
    7890
}

fn main() {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("canned error: failed to initialize tokio runtime: {err}");
            process::exit(1);
        }
    };

    if let Err(err) = rt.block_on(run_server()) {
        eprintln!("canned error: {err}");
        process::exit(1);
    }
}

async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = get_config_path();
    let yaml_content = fs::read_to_string(&config_path)
        .map_err(|e| format!("failed to read config file at '{config_path}': {e}"))?;

    let router = Arc::new(Router::from_yaml(&yaml_content)?);

    let port = get_port();
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;

    eprintln!("canned listening on http://{addr} (loaded config from '{config_path}')");

    loop {
        tokio::select! {
            accept_res = listener.accept() => {
                let (stream, _) = match accept_res {
                    Ok(conn) => conn,
                    Err(e) => {
                        eprintln!("accept error: {e}");
                        continue;
                    }
                };
                let io = TokioIo::new(stream);
                let router = Arc::clone(&router);

                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let resp = handle_request(req, &router);
                        async move { Ok::<_, Infallible>(resp) }
                    });

                    if let Err(err) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                        && !err.is_incomplete_message() {
                            eprintln!("connection error: {err}");
                        }
                });
            }
            _ = shutdown_signal() => {
                eprintln!("shutting down");
                break;
            }
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sigterm) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sigterm.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_YAML: &str = r#"
fullUrl:
  http://example.com/override:
    status: 201
    headers:
      X-Custom: "yes"
    content: "Full URL matched"
host:
  example.com:
    status: 302
    headers:
      Location: https://www.example.com
path:
  /robots.txt:
    status: 200
    headers:
      Content-Type: text/plain
    content: "Disallow: *"
default:
  status: 404
  content: "Not Found"
"#;

    #[test]
    fn test_priority_fullurl_then_host_then_path_then_none() {
        let router = Router::from_yaml(EXAMPLE_YAML).expect("valid yaml");

        // 1. Matching fullurl beats matching host even though host rule is also matched
        let resp = router.resolve("http", Some("example.com"), "/override", "/override");
        assert_eq!(resp.status, StatusCode::CREATED);
        assert_eq!(resp.headers.get("x-custom").unwrap(), "yes");
        assert_eq!(resp.body, Bytes::from("Full URL matched"));

        // 2. Matching host beats matching path (/robots.txt matches both host and path)
        let resp = router.resolve("http", Some("example.com"), "/robots.txt", "/robots.txt");
        assert_eq!(resp.status, StatusCode::FOUND);
        assert_eq!(
            resp.headers.get("location").unwrap(),
            "https://www.example.com"
        );

        // 3. Matching host with port in Host header
        let resp = router.resolve(
            "http",
            Some("example.com:7890"),
            "/anything",
            "/anything",
        );
        assert_eq!(resp.status, StatusCode::FOUND);

        // 4. Matching path when host does not match
        let resp = router.resolve("http", Some("other.org"), "/robots.txt", "/robots.txt");
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.headers.get("content-type").unwrap(), "text/plain");
        assert_eq!(resp.body, Bytes::from("Disallow: *"));

        // 5. Fallback to default rule (none)
        let resp = router.resolve("http", Some("other.org"), "/missing", "/missing");
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
        assert_eq!(resp.body, Bytes::from("Not Found"));
    }

    #[test]
    fn test_various_header_and_url_formats() {
        let yaml = r#"
fullUrl:
  example.com/dict-headers:
    status: 200
    headers:
      X-Single: "value1"
      X-Number: 42
      X-List:
        - "valA"
        - "valB"
    content: "dict-ok"
path:
  /combined:
    status: 202
    headers:
      Content-Type: application/json
    content: '{"ok":true}'
"#;
        let router = Router::from_yaml(yaml).expect("valid yaml");

        let r1 = router.resolve("https", Some("example.com"), "/dict-headers", "/dict-headers");
        assert_eq!(r1.status, StatusCode::OK);
        assert_eq!(r1.headers.get("x-single").unwrap(), "value1");
        assert_eq!(r1.headers.get("x-number").unwrap(), "42");
        let list_headers: Vec<_> = r1.headers.get_all("x-list").iter().map(|v| v.to_str().unwrap()).collect();
        assert_eq!(list_headers, vec!["valA", "valB"]);

        let r2 = router.resolve("http", Some("sub.example.com"), "/combined", "/combined");
        assert_eq!(r2.status, StatusCode::ACCEPTED);
        assert_eq!(r2.body, Bytes::from(r#"{"ok":true}"#));
    }

    #[test]
    fn test_rejects_header_list() {
        let yaml = r#"
path:
  /invalid:
    status: 200
    headers:
      - Content-Type: text/plain
    content: "bad"
"#;
        let res = Router::from_yaml(yaml);
        assert!(res.is_err(), "should reject list of headers");
    }

    #[test]
    fn test_rejects_old_list_format() {
        let yaml = r#"
- path: /robots.txt
  status: 200
  content: "Disallow: *"
- statusCode: 404
"#;
        let res = Router::from_yaml(yaml);
        assert!(res.is_err(), "should reject old list format");
    }
}
