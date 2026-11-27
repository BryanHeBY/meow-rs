use std::future::{poll_fn, Future};
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use meow_common::{route_check, AdapterType, ConnType, Metadata, Proxy};
use meow_transport::tls::{TlsConfig, TlsLayer};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Semaphore;
use tokio::time::{sleep_until, timeout_at, Instant};

use crate::routes::{token_matches, AppState};

const MAX_TIMEOUT_MS: u32 = 2_000;
const MAX_REQUEST_BYTES: usize = 4 * 1024;
const MAX_HEADER_BYTES: usize = 4 * 1024;
const MAX_LABEL_BYTES: usize = 256;
const MAX_ACTIVE: usize = 4;
const MAX_ADMITTED: usize = 8;

pub(crate) fn is_loopback(addr: SocketAddr) -> bool {
    addr.ip().to_canonical().is_loopback()
}

/// Installed only on the dedicated plain TCP router after verifying its bind.
pub(crate) struct LoopbackController {
    session: u64,
    admitted: Semaphore,
    active: Semaphore,
    #[cfg(test)]
    test_root: Option<Vec<u8>>,
}

impl LoopbackController {
    pub(crate) fn new() -> Self {
        let id = uuid::Uuid::new_v4().into_bytes();
        // Use the UUID half with only the two reserved variant bits.
        let session = u64::from_ne_bytes(id[8..].try_into().unwrap());
        Self {
            session,
            admitted: Semaphore::new(MAX_ADMITTED),
            active: Semaphore::new(MAX_ACTIVE),
            #[cfg(test)]
            test_root: None,
        }
    }

    pub(crate) fn authorized<'a>(
        state: &AppState,
        req: &'a Request,
    ) -> Result<&'a Arc<Self>, StatusCode> {
        let extensions = req.extensions();
        let controller = extensions.get::<Arc<Self>>().ok_or(StatusCode::NOT_FOUND)?;
        if !extensions
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|peer| is_loopback(peer.0))
            || !state.auth_required()
        {
            return Err(StatusCode::NOT_FOUND);
        }
        let mut authorization = req.headers().get_all(header::AUTHORIZATION).iter();
        let bearer = authorization
            .next()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if authorization.next().is_some()
            || !bearer
                .zip(state.secret.as_deref())
                .is_some_and(|(token, secret)| token_matches(token, secret))
        {
            return Err(StatusCode::UNAUTHORIZED);
        }
        Ok(controller)
    }

    fn generation(&self, revision: route_check::Revision) -> Option<u64> {
        revision
            .current()
            .map(|number| self.session.wrapping_add(number))
    }

    pub(crate) fn capabilities(&self) -> Option<Capabilities> {
        let generation = self.generation(*route_check::subscribe().borrow())?;
        Some(Capabilities {
            route_http_check_v1: RouteCapability { generation },
        })
    }
}

#[derive(Serialize)]
pub(crate) struct Capabilities {
    #[serde(rename = "route-http-check-v1")]
    route_http_check_v1: RouteCapability,
}

#[derive(Serialize)]
struct RouteCapability {
    generation: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Route {
    name: String,
    // Explicit null identifies the local namespace. Missing is unqualified.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    provider: Option<String>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Check {
    Gstatic204,
    Cloudflare204,
}

impl Check {
    fn host(self) -> &'static str {
        match self {
            Self::Gstatic204 => "www.gstatic.com",
            Self::Cloudflare204 => "cp.cloudflare.com",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRequest {
    generation: u64,
    route: Route,
    check: Check,
    timeout_ms: u32,
}

#[derive(Serialize)]
struct CheckReply {
    route: Route,
    check: Check,
    status: u16,
    elapsed_ms: u32,
}

pub(crate) async fn check(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Result<Response, StatusCode> {
    let start = Instant::now();
    let controller = Arc::clone(LoopbackController::authorized(&state, &req)?);
    if req.uri().query().is_some() || req.headers().contains_key(header::CONTENT_ENCODING) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let _admission = controller
        .admitted
        .try_acquire()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let bytes = timeout_at(
        start + Duration::from_millis(u64::from(MAX_TIMEOUT_MS)),
        to_bytes(req.into_body(), MAX_REQUEST_BYTES),
    )
    .await
    .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
    .map_err(|_| StatusCode::BAD_REQUEST)?;
    let request: CheckRequest =
        serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
    if !(1..=MAX_TIMEOUT_MS).contains(&request.timeout_ms)
        || !valid_label(&request.route.name)
        || !request.route.provider.as_deref().is_none_or(valid_label)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let deadline = start + Duration::from_millis(u64::from(request.timeout_ms));
    let status = run(&state, &controller, &request, deadline).await?;
    let end = Instant::now();
    if end >= deadline {
        return Err(StatusCode::GATEWAY_TIMEOUT);
    }
    let elapsed_ms = u32::try_from((end - start).as_millis()).unwrap_or(u32::MAX);
    let reply = CheckReply {
        route: request.route,
        check: request.check,
        status,
        elapsed_ms,
    };
    Ok((
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(reply),
    )
        .into_response())
}

fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= MAX_LABEL_BYTES && !label.chars().any(char::is_control)
}

async fn run(
    state: &AppState,
    controller: &LoopbackController,
    request: &CheckRequest,
    deadline: Instant,
) -> Result<u16, StatusCode> {
    let mut revision = route_check::subscribe();
    let current = revision.clone();
    let is_current = |value| controller.generation(value) == Some(request.generation);
    if !is_current(*current.borrow()) {
        return Err(StatusCode::CONFLICT);
    }
    tokio::select! {
        biased;
        () = sleep_until(deadline) => Err(StatusCode::GATEWAY_TIMEOUT),
        _ = revision.changed() => Err(StatusCode::CONFLICT),
        result = async {
            let _active = controller.active.acquire().await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            let adapter = {
                let guard = current.borrow();
                if !is_current(*guard) {
                    return Err(StatusCode::CONFLICT);
                }
                resolve(state, &request.route)?
            };
            let host = request.check.host();
            // Same name for SNI and certificate verification. An explicit
            // verify name also keeps this check out of the shared TLS session cache.
            let config = TlsConfig { verify_name: Some(host.to_owned()), ..TlsConfig::new(host) };
            #[cfg(test)]
            let config = TlsConfig {
                additional_roots: controller.test_root.iter().cloned().collect(),
                ..config
            };
            let exchange = route_check::scope(exchange(&*adapter, request.check, &config));
            tokio::pin!(exchange);
            poll_fn(|cx| {
                // Linearize the first dial poll and every later I/O poll
                // against publication. No read guard survives a yield.
                let guard = current.borrow();
                if !is_current(*guard) {
                    return Poll::Ready(Err(StatusCode::CONFLICT));
                }
                if Instant::now() >= deadline {
                    return Poll::Ready(Err(StatusCode::GATEWAY_TIMEOUT));
                }
                exchange.as_mut().poll(cx)
            }).await
        } => result,
    }
}

fn resolve(state: &AppState, route: &Route) -> Result<Arc<dyn Proxy>, StatusCode> {
    let rejected = StatusCode::UNPROCESSABLE_ENTITY;
    let proxy = if let Some(name) = &route.provider {
        let provider = state.proxy_providers.get(name).ok_or(rejected)?;
        let members = provider.slot.read();
        let mut named = members.iter().filter(|proxy| proxy.name() == route.name);
        let proxy = named
            .next()
            .filter(|_| named.next().is_none())
            .ok_or(rejected)?;
        Arc::clone(proxy)
    } else {
        // The runtime map is last-wins for duplicate local declarations.
        // A diagnostic must refuse those even though normal routing works.
        let raw = state.raw_config.read();
        let mut named = raw.proxies.iter().flatten().filter(|proxy| {
            proxy.get("name").and_then(serde_json::Value::as_str) == Some(route.name.as_str())
        });
        if named.next().is_none() || named.next().is_some() {
            return Err(rejected);
        }
        state.tunnel.proxy(&route.name).ok_or(rejected)?
    };
    if proxy.addr().is_empty()
        || proxy.has_configured_dialer()
        || !matches!(
            proxy.adapter_type(),
            AdapterType::Shadowsocks
                | AdapterType::Socks5
                | AdapterType::Http
                | AdapterType::Vmess
                | AdapterType::Vless
                | AdapterType::Trojan
                | AdapterType::Hysteria2
                | AdapterType::TrustTunnel
                | AdapterType::Anytls
                | AdapterType::Snell
                | AdapterType::WireGuard
        )
    {
        return Err(rejected);
    }
    Ok(proxy)
}

async fn exchange(
    adapter: &dyn Proxy,
    check: Check,
    config: &TlsConfig,
) -> Result<u16, StatusCode> {
    let failed = StatusCode::BAD_GATEWAY;
    let tls = TlsLayer::new(config).map_err(|_| failed)?;
    let metadata = Metadata {
        conn_type: ConnType::Https,
        host: check.host().into(),
        dst_port: 443,
        internal: true,
        ..Default::default()
    };
    // The adapter resolves its own origin and carries or resolves the target
    // hostname according to its protocol, without a tunnel rule decision.
    let conn = adapter.dial_tcp(&metadata).await.map_err(|_| failed)?;
    let mut stream = tls.connect_typed(conn).await.map_err(|_| failed)?;
    request_status(&mut stream, check).await.map_err(|_| failed)
}

async fn request_status(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    check: Check,
) -> std::io::Result<u16> {
    let request = format!(
        "GET /generate_204 HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept-Encoding: identity\r\n\r\n",
        check.host()
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    // Consume exactly through the final header delimiter, even when the
    // peer sends a body in the same TLS record. Count interim headers too.
    let mut bytes = [0; MAX_HEADER_BYTES];
    let mut start = 0;
    for end in 1..=bytes.len() {
        stream.read_exact(&mut bytes[end - 1..end]).await?;
        let head = &bytes[start..end];
        if !head.ends_with(b"\r\n\r\n") {
            continue;
        }
        let mut headers = vec![httparse::EMPTY_HEADER; head.len() / 4];
        let mut response = httparse::Response::new(&mut headers);
        if !matches!(response.parse(head), Ok(httparse::Status::Complete(len)) if len == head.len())
        {
            break;
        }
        match response.code {
            Some(code @ 200..=599) => return Ok(code),
            Some(100 | 102..=199) => start = end,
            _ => break,
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid or oversized HTTP response head",
    ))
}

#[cfg(test)]
mod tests;
