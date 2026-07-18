use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes, Frame, Incoming, SizeHint};
use hyper::header::{
    AUTHORIZATION, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderMap, HeaderName,
    HeaderValue,
};
use hyper::{Method, Request, Response, StatusCode, Uri};
use tokio::time::Sleep;

use crate::config::{Config, Provider};

type HttpsConnector =
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>;
pub type Client = hyper_util::client::legacy::Client<HttpsConnector, CappedBody>;
type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type ResBody = BoxBody<Bytes, BoxError>;

const ALLOWED: [&str; 3] = ["v1/chat/completions", "v1/messages", "v1/responses"];
const HOP_BY_HOP: [&str; 6] = [
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];
pub const KEY_CMP_MAX: usize = 256;

pub struct Gateway {
    pub listen: SocketAddr,
    keys: Vec<String>,
    max_body: usize,
    upstream_header_timeout: Duration,
    upstream_idle_timeout: Duration,
    pub header_read_timeout: Duration,
    pub routes: BTreeMap<String, Upstream>,
    client: Client,
}

pub struct Upstream {
    base_url: String,
    auth_name: HeaderName,
    auth_value: HeaderValue,
}

impl Gateway {
    pub fn new(cfg: Config, client: Client) -> Result<Self, String> {
        let routes = build_routes(&cfg.providers)?;
        Ok(Self {
            listen: cfg.listen,
            keys: cfg.gateway_keys,
            max_body: cfg.max_body_bytes,
            upstream_header_timeout: Duration::from_secs(cfg.upstream_header_timeout_secs),
            upstream_idle_timeout: Duration::from_secs(cfg.upstream_idle_timeout_secs),
            header_read_timeout: Duration::from_secs(cfg.header_read_timeout_secs),
            routes,
            client,
        })
    }
}

fn build_routes(
    providers: &BTreeMap<String, Provider>,
) -> Result<BTreeMap<String, Upstream>, String> {
    providers
        .iter()
        .map(|(name, p)| {
            let auth_name = HeaderName::try_from(p.auth_header.as_str())
                .map_err(|e| format!("providers.{name}: auth_header: {e}"))?;
            let raw = if auth_name == AUTHORIZATION {
                format!("Bearer {}", p.api_key)
            } else {
                p.api_key.clone()
            };
            let mut auth_value = HeaderValue::try_from(raw)
                .map_err(|e| format!("providers.{name}: api_key: {e}"))?;
            auth_value.set_sensitive(true);
            let upstream = Upstream {
                base_url: p.base_url.trim_end_matches('/').to_string(),
                auth_name,
                auth_value,
            };
            Ok((name.clone(), upstream))
        })
        .collect()
}

pub async fn handle(gw: &Gateway, req: Request<Incoming>) -> Response<ResBody> {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let resp = route(gw, req).await;
    if path != "/healthz" {
        eprintln!(
            "{method} {path} {} {}ms",
            resp.status().as_u16(),
            started.elapsed().as_millis()
        );
    }
    resp
}

async fn route(gw: &Gateway, req: Request<Incoming>) -> Response<ResBody> {
    if req.method() == Method::GET && req.uri().path() == "/healthz" {
        return text(StatusCode::OK, "ok");
    }
    if !authorized(req.headers(), &gw.keys) {
        return text(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let Some((upstream, endpoint)) = match_route(&gw.routes, req.method(), req.uri().path()) else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    if declared_len(req.headers()).is_some_and(|n| n > gw.max_body) {
        return text(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
    }
    forward(gw, upstream, endpoint, req).await
}

async fn forward(
    gw: &Gateway,
    up: &Upstream,
    endpoint: &'static str,
    req: Request<Incoming>,
) -> Response<ResBody> {
    let (mut parts, body) = req.into_parts();
    let Some(uri) = upstream_uri(&up.base_url, endpoint, parts.uri.query()) else {
        return text(StatusCode::BAD_GATEWAY, "upstream error");
    };
    sanitize(&mut parts.headers);
    parts.headers.remove(HOST);
    parts.headers.remove(AUTHORIZATION);
    parts
        .headers
        .insert(up.auth_name.clone(), up.auth_value.clone());
    parts.uri = uri;
    let tripped = Arc::new(AtomicBool::new(false));
    let body = CappedBody {
        inner: body,
        remaining: gw.max_body,
        tripped: tripped.clone(),
    };
    let req = Request::from_parts(parts, body);

    // timeout covers response headers only, bodies stream undeadlined (SSE)
    match tokio::time::timeout(gw.upstream_header_timeout, gw.client.request(req)).await {
        Ok(Ok(resp)) => {
            let (mut parts, body) = resp.into_parts();
            sanitize(&mut parts.headers);
            Response::from_parts(parts, IdleBody::new(body, gw.upstream_idle_timeout).boxed())
        }
        Ok(Err(_)) if tripped.load(Ordering::Relaxed) => {
            text(StatusCode::PAYLOAD_TOO_LARGE, "body too large")
        }
        Ok(Err(e)) => {
            eprintln!("upstream error: {e}");
            text(StatusCode::BAD_GATEWAY, "upstream error")
        }
        Err(_) => text(StatusCode::GATEWAY_TIMEOUT, "upstream timeout"),
    }
}

pub struct CappedBody {
    inner: Incoming,
    remaining: usize,
    tripped: Arc<AtomicBool>,
}

impl Body for CappedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        Poll::Ready(match ready!(Pin::new(&mut this.inner).poll_frame(cx)) {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    if data.len() > this.remaining {
                        this.tripped.store(true, Ordering::Relaxed);
                        return Poll::Ready(Some(Err("body too large".into())));
                    }
                    this.remaining -= data.len();
                }
                Some(Ok(frame))
            }
            Some(Err(e)) => Some(Err(e.into())),
            None => None,
        })
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

struct IdleBody<B> {
    inner: B,
    idle: Duration,
    sleep: Pin<Box<Sleep>>,
}

impl<B> IdleBody<B> {
    fn new(inner: B, idle: Duration) -> Self {
        Self {
            inner,
            idle,
            sleep: Box::pin(tokio::time::sleep(idle)),
        }
    }
}

impl<B: Body<Data = Bytes> + Unpin> Body for IdleBody<B>
where
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if let Poll::Ready(opt) = Pin::new(&mut this.inner).poll_frame(cx) {
            this.sleep
                .as_mut()
                .reset(tokio::time::Instant::now() + this.idle);
            return Poll::Ready(opt.map(|r| r.map_err(Into::into)));
        }
        ready!(this.sleep.as_mut().poll(cx));
        Poll::Ready(Some(Err("upstream idle timeout".into())))
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

fn authorized(headers: &HeaderMap, keys: &[String]) -> bool {
    let token = headers
        .get(AUTHORIZATION)
        .map(HeaderValue::as_bytes)
        .and_then(bearer_token)
        .unwrap_or_default();
    keys.iter()
        .fold(false, |ok, k| ok | ct_eq(k.as_bytes(), token))
}

fn bearer_token(v: &[u8]) -> Option<&[u8]> {
    let end = v.iter().position(|&b| b == b' ')?;
    v[..end]
        .eq_ignore_ascii_case(b"bearer")
        .then_some(&v[end + 1..])
}

// constant-time, length-hiding: do not branch on either length.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..KEY_CMP_MAX {
        diff |=
            (a.get(i).copied().unwrap_or(0) as usize) ^ (b.get(i).copied().unwrap_or(0) as usize);
    }
    diff == 0
}

fn match_route<'r>(
    routes: &'r BTreeMap<String, Upstream>,
    method: &Method,
    path: &str,
) -> Option<(&'r Upstream, &'static str)> {
    if method != Method::POST {
        return None;
    }
    let (name, rest) = parse_path(path)?;
    let endpoint = ALLOWED.iter().find(|p| **p == rest).copied()?;
    Some((routes.get(name)?, endpoint))
}

fn parse_path(path: &str) -> Option<(&str, &str)> {
    let path = path.strip_prefix('/')?;
    let (name, rest) = path.split_at(path.find('/')?);
    (!name.is_empty()).then_some((name, &rest[1..]))
}

fn upstream_uri(base_url: &str, endpoint: &str, query: Option<&str>) -> Option<Uri> {
    let q = query.map(|q| format!("?{q}")).unwrap_or_default();
    format!("{base_url}/{endpoint}{q}").parse().ok()
}

fn sanitize(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|t| HeaderName::try_from(t.trim()).ok())
        .collect();
    let drop: Vec<HeaderName> = headers
        .keys()
        .filter(|k| {
            named.contains(k)
                || HOP_BY_HOP.contains(&k.as_str())
                || k.as_str().starts_with("proxy-")
        })
        .cloned()
        .collect();
    for h in drop {
        headers.remove(h);
    }
}

fn declared_len(headers: &HeaderMap) -> Option<usize> {
    headers.get(CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

fn text(status: StatusCode, msg: &'static str) -> Response<ResBody> {
    let body = Full::new(Bytes::from_static(msg.as_bytes()))
        .map_err(|e: std::convert::Infallible| -> BoxError { match e {} })
        .boxed();
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain")
        .body(body)
        .expect("static response")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routes() -> BTreeMap<String, Upstream> {
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".to_string(),
            Provider {
                base_url: "https://api.anthropic.com/".into(),
                auth_header: "x-api-key".into(),
                api_key: "sk-ant".into(),
            },
        );
        providers.insert(
            "openai".to_string(),
            Provider {
                base_url: "https://api.openai.com".into(),
                auth_header: "authorization".into(),
                api_key: "sk-oai".into(),
            },
        );
        build_routes(&providers).unwrap()
    }

    #[test]
    fn build_routes_composes_bearer_and_trims_slash() {
        let r = routes();
        assert_eq!(r["anthropic"].base_url, "https://api.anthropic.com");
        assert_eq!(r["anthropic"].auth_value, "sk-ant");
        assert_eq!(r["openai"].auth_value, "Bearer sk-oai");
        assert!(r["openai"].auth_value.is_sensitive());
    }

    #[test]
    fn build_routes_rejects_bad_auth_header() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "x".to_string(),
            Provider {
                base_url: "https://x.test".into(),
                auth_header: "not a header".into(),
                api_key: "k".into(),
            },
        );
        match build_routes(&providers) {
            Err(e) => assert!(e.contains("auth_header")),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn matches_allowlisted_posts_only() {
        let r = routes();
        for p in [
            "/anthropic/v1/messages",
            "/openai/v1/chat/completions",
            "/openai/v1/responses",
        ] {
            assert!(match_route(&r, &Method::POST, p).is_some(), "{p}");
        }
        for p in [
            "/anthropic/v1/complete",
            "/anthropic/v1/messages/batches",
            "/unknown/v1/messages",
            "/anthropic",
            "/",
            "//v1/messages",
        ] {
            assert!(match_route(&r, &Method::POST, p).is_none(), "{p}");
        }
        assert!(match_route(&r, &Method::GET, "/anthropic/v1/messages").is_none());
    }

    #[test]
    fn auth_accepts_any_configured_key() {
        let keys = vec![
            "0123456789abcdef".to_string(),
            "fedcba9876543210".to_string(),
        ];
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, "Bearer fedcba9876543210".parse().unwrap());
        assert!(authorized(&h, &keys));
        h.insert(AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert!(!authorized(&h, &keys));
        h.insert(AUTHORIZATION, "0123456789abcdef".parse().unwrap());
        assert!(!authorized(&h, &keys), "missing Bearer prefix");
        h.remove(AUTHORIZATION);
        assert!(!authorized(&h, &keys));
    }

    #[test]
    fn ct_eq_compares_without_length_leak() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(ct_eq(b"", b""));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(!ct_eq(b"ab", b"abc"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"abcd", b"abc"));
        assert!(!ct_eq(b"abc", b""));
        assert!(!ct_eq(b"", b"abc"));
    }

    #[test]
    fn sanitize_strips_hop_by_hop_and_connection_named() {
        let mut h = HeaderMap::new();
        h.insert(CONNECTION, "keep-alive, x-secret-hop".parse().unwrap());
        h.insert("x-secret-hop", "1".parse().unwrap());
        h.insert("transfer-encoding", "chunked".parse().unwrap());
        h.insert("proxy-connection", "keep-alive".parse().unwrap());
        h.insert("anthropic-version", "2023-06-01".parse().unwrap());
        sanitize(&mut h);
        assert_eq!(h.len(), 1);
        assert!(h.contains_key("anthropic-version"));
    }

    #[test]
    fn upstream_uri_preserves_query() {
        let u = upstream_uri("https://x.test", "v1/messages", Some("beta=true")).unwrap();
        assert_eq!(u.to_string(), "https://x.test/v1/messages?beta=true");
        let u = upstream_uri("https://x.test", "v1/messages", None).unwrap();
        assert_eq!(u.to_string(), "https://x.test/v1/messages");
    }

    struct Stuck;

    impl Body for Stuck {
        type Data = Bytes;
        type Error = BoxError;

        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn idle_body_errors_on_stuck_upstream() {
        assert!(
            IdleBody::new(Stuck, Duration::from_millis(20))
                .collect()
                .await
                .is_err(),
            "expected idle timeout"
        );
    }

    #[tokio::test]
    async fn idle_body_passes_frames() {
        let c = IdleBody::new(
            Full::new(Bytes::from_static(b"hi")),
            Duration::from_secs(60),
        )
        .collect()
        .await
        .unwrap();
        assert_eq!(c.to_bytes().as_ref(), &b"hi"[..]);
    }

    #[test]
    fn auth_accepts_case_insensitive_scheme() {
        let keys = vec!["0123456789abcdef".to_string()];
        for scheme in ["bearer", "BEARER", "Bearer"] {
            let mut h = HeaderMap::new();
            h.insert(
                AUTHORIZATION,
                format!("{scheme} 0123456789abcdef").parse().unwrap(),
            );
            assert!(authorized(&h, &keys), "{scheme}");
        }
    }
}
