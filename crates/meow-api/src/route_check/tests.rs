use super::*;
use crate::routes::create_router;
use axum::body::Body;
use boring::asn1::{Asn1Integer, Asn1Time};
use boring::bn::BigNum;
use boring::hash::MessageDigest;
use boring::pkey::PKey;
use boring::rsa::Rsa;
use boring::ssl::{NameType, SslAcceptor, SslMethod};
use boring::x509::{extension::SubjectAlternativeName, X509Name, X509};
use meow_common::{DelayHistory, ProxyAdapter, ProxyConn, ProxyHealth, ProxyPacketConn};
use meow_config::proxy_provider::ProxyProvider;
use meow_tunnel::Tunnel;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::Context;
use tokio::io::{duplex, DuplexStream, ReadBuf};
use tokio::sync::mpsc;
use tower::ServiceExt;

const SECRET: &str = "Bearer local-test-secret";
const ROUTE: &str = "/api/unlock/check";
const UPSTREAM_204: &[u8] = b"HTTP/1.1 204 No Content\r\n\r\n";
const UPSTREAM_REDIRECT: &[u8] =
    b"HTTP/1.1 307 Temporary Redirect\r\nLocation: https://not-the-check.invalid/\r\n\r\n";

struct MockProxy {
    name: &'static str,
    kind: AdapterType,
    chained: bool,
    health: ProxyHealth,
    actions: Mutex<VecDeque<Action>>,
    dials: mpsc::UnboundedSender<Metadata>,
}

enum Action {
    Stream(DuplexStream),
    Pending,
    Fail,
}

impl MockProxy {
    fn new(
        name: &'static str,
        kind: AdapterType,
        chained: bool,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<Metadata>) {
        let (dials, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                name,
                kind,
                chained,
                health: ProxyHealth::new(),
                actions: Mutex::default(),
                dials,
            }),
            receiver,
        )
    }
}

#[async_trait::async_trait]
impl ProxyAdapter for MockProxy {
    fn name(&self) -> &str {
        self.name
    }
    fn adapter_type(&self) -> AdapterType {
        self.kind
    }
    fn addr(&self) -> &str {
        "proxy-fixture.invalid:1080"
    }
    fn support_udp(&self) -> bool {
        false
    }
    async fn dial_tcp(&self, metadata: &Metadata) -> meow_common::Result<Box<dyn ProxyConn>> {
        self.dials.send(metadata.clone()).unwrap();
        let action = self
            .actions
            .lock()
            .pop_front()
            .expect("unexpected adapter dial");
        match action {
            Action::Stream(stream) => Ok(Box::new(Pipe(stream))),
            Action::Pending => std::future::pending().await,
            Action::Fail => Err(meow_common::MeowError::Proxy("fixture dial failure".into())),
        }
    }
    async fn dial_udp(&self, _: &Metadata) -> meow_common::Result<Box<dyn ProxyPacketConn>> {
        panic!("a route check attempted UDP")
    }
    fn has_configured_dialer(&self) -> bool {
        self.chained
    }
    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

impl Proxy for MockProxy {
    fn alive(&self) -> bool {
        self.health.alive()
    }
    fn alive_for_url(&self, _: &str) -> bool {
        self.alive()
    }
    fn last_delay(&self) -> u16 {
        self.health.last_delay()
    }
    fn last_delay_for_url(&self, _: &str) -> u16 {
        self.last_delay()
    }
    fn delay_history(&self) -> Vec<DelayHistory> {
        self.health.delay_history()
    }
}

struct Pipe(DuplexStream);

impl ProxyConn for Pipe {}

impl AsyncRead for Pipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Pipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

fn fixture_state(secret: Option<&str>) -> AppState {
    let resolver = meow_dns::Resolver::new(
        vec![],
        vec![],
        meow_common::DnsMode::Normal,
        meow_trie::DomainTrie::new(),
        true,
        false,
    );
    AppState {
        tunnel: Tunnel::new(Arc::new(resolver)),
        secret: secret.map(str::to_owned),
        raw_config: Arc::new(RwLock::new(meow_config::parse_raw_yaml(
            "proxies: [{name: Local}, {name: Chained}, {name: Direct alias}, {name: Duplicate}, {name: Duplicate}]",
        ).unwrap())),
        config_path: None,
        log_tx: tokio::sync::broadcast::channel(1).0,
        proxy_providers: Default::default(),
        provider_dialer_registry: Default::default(),
        rule_providers: Default::default(),
        rule_provider_refresh: Default::default(),
        proxy_provider_refresh: Default::default(),
        listeners: vec![],
        external_ui: None,
        traffic_feed: Default::default(),
        dns_server: Default::default(),
        ui_storage: Default::default(),
        ntp_reconcile: None,
        managed_fallback_geoip: None,
        cors: Default::default(),
    }
}

fn controller_router(
    state: &Arc<AppState>,
    controller: &Arc<LoopbackController>,
    peer: &str,
) -> axum::Router {
    create_router(Arc::clone(state))
        .layer(axum::Extension(Arc::clone(controller)))
        .layer(axum::Extension(ConnectInfo(
            peer.parse::<SocketAddr>().unwrap(),
        )))
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Body,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(bearer) = bearer {
        request = request.header(header::AUTHORIZATION, bearer);
    }
    let response = router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), MAX_REQUEST_BYTES)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn generation(router: &axum::Router) -> u64 {
    let (status, response) = send(router, "GET", "/version", Some(SECRET), Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["meta"], true);
    response["capabilities"]["route-http-check-v1"]["generation"]
        .as_u64()
        .unwrap()
}

fn payload(generation: u64, name: &str, provider: Option<&str>, check: &str) -> Value {
    json!({
        "generation": generation,
        "route": {"name": name, "provider": provider},
        "check": check,
        "timeout_ms": MAX_TIMEOUT_MS,
    })
}

async fn check_route(router: &axum::Router, request: &Value) -> (StatusCode, Value) {
    send(
        router,
        "POST",
        ROUTE,
        Some(SECRET),
        Body::from(request.to_string()),
    )
    .await
}

fn certificate() -> (Vec<u8>, SslAcceptor) {
    let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let mut name = X509Name::builder().unwrap();
    name.append_entry_by_text("CN", "route check fixture")
        .unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_serial_number(&Asn1Integer::from_bn(&BigNum::from_u32(1).unwrap()).unwrap())
        .unwrap();
    cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    let mut names = SubjectAlternativeName::new();
    names
        .dns(Check::Gstatic204.host())
        .dns(Check::Cloudflare204.host());
    let san = names.build(&cert.x509v3_context(None, None)).unwrap();
    cert.append_extension(&san).unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    let cert = cert.build();
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor.set_certificate(&cert).unwrap();
    acceptor.set_private_key(&key).unwrap();
    (cert.to_der().unwrap(), acceptor.build())
}

async fn read_request(stream: &mut (impl AsyncRead + Unpin)) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 1024);
        bytes.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(bytes).unwrap()
}

fn attach_tls(
    proxy: &MockProxy,
    acceptor: SslAcceptor,
    check: Check,
    response: &'static [u8],
) -> tokio::task::JoinHandle<()> {
    let (client, server) = duplex(16 * 1024);
    proxy.actions.lock().push_back(Action::Stream(client));
    tokio::spawn(async move {
        let mut stream = tokio_boring::accept(&acceptor, server).await.unwrap();
        assert_eq!(
            stream.ssl().servername(NameType::HOST_NAME),
            Some(check.host())
        );
        assert_eq!(read_request(&mut stream).await, format!(
            "GET /generate_204 HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept-Encoding: identity\r\n\r\n",
            check.host(),
        ));
        stream.write_all(response).await.unwrap();
        stream.flush().await.unwrap();
    })
}

#[tokio::test(start_paused = true)]
async fn paw_route_check_contract() {
    let _config_lane = crate::routes::CONFIG_MUTATION.lock().await;
    let (root, tls) = certificate();
    let controller = Arc::new(LoopbackController {
        test_root: Some(root),
        ..LoopbackController::new()
    });
    let (local, mut local_dials) = MockProxy::new("Local", AdapterType::Socks5, false);
    let (remote, mut remote_dials) = MockProxy::new("Local", AdapterType::Socks5, false);
    let (direct, mut direct_dials) = MockProxy::new("Direct alias", AdapterType::Direct, false);
    let (chained, mut chained_dials) = MockProxy::new("Chained", AdapterType::Socks5, true);
    let (duplicate, mut duplicate_dials) = MockProxy::new("Duplicate", AdapterType::Socks5, false);
    let local_proxy: Arc<dyn Proxy> = Arc::<MockProxy>::clone(&local);
    let group = Arc::new(meow_proxy::SelectorGroup::new(
        "Choice",
        vec![Arc::clone(&local_proxy)],
    ));
    let duplicate_proxy: Arc<dyn Proxy> = duplicate;
    let proxies: [Arc<dyn Proxy>; 5] = [
        local_proxy,
        Arc::<meow_proxy::SelectorGroup>::clone(&group),
        direct,
        chained,
        Arc::clone(&duplicate_proxy),
    ];
    let state = Arc::new(fixture_state(Some("local-test-secret")));
    state.tunnel.update_proxies(
        proxies
            .into_iter()
            .map(|proxy| (proxy.name().into(), proxy))
            .collect(),
        Default::default(),
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("provider.yml");
    std::fs::write(
        &path,
        "proxies: [{name: Local, type: socks5, server: 192.0.2.1, port: 1080}]",
    )
    .unwrap();
    let provider = Arc::new(
        ProxyProvider::new(
            "Fixture provider",
            &serde_saphyr::from_str(&format!("type: file\npath: {}", path.display())).unwrap(),
            Some(directory.path()),
            false,
            true,
            Default::default(),
        )
        .unwrap(),
    );
    let provider_proxy: Arc<dyn Proxy> = Arc::<MockProxy>::clone(&remote);
    *provider.slot.write() = vec![
        provider_proxy,
        Arc::clone(&duplicate_proxy),
        duplicate_proxy,
    ];
    state
        .proxy_providers
        .insert(provider.name.clone(), Arc::clone(&provider));

    let router = controller_router(&state, &controller, "127.0.0.1:1080");
    let initial = generation(&router).await;
    let gstatic = payload(initial, "Local", None, "gstatic204");
    let cloudflare = payload(initial, "Local", Some(&provider.name), "cloudflare204");

    for surface in [
        create_router(Arc::clone(&state)),
        controller_router(&state, &controller, "192.0.2.5:1080"),
        controller_router(&Arc::new(fixture_state(None)), &controller, "[::1]:1080"),
    ] {
        let (status, response) =
            send(&surface, "GET", "/version", Some(SECRET), Body::empty()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(response.get("capabilities").is_none());
        assert_eq!(
            check_route(&surface, &gstatic).await.0,
            StatusCode::NOT_FOUND
        );
    }
    for bearer in [None, Some("Bearer wrong")] {
        let (status, _) = send(
            &router,
            "POST",
            ROUTE,
            bearer,
            Body::from(gstatic.to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        send(
            &router,
            "POST",
            "/api/unlock/check?token=local-test-secret",
            None,
            Body::from(gstatic.to_string())
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&router, "GET", ROUTE, Some(SECRET), Body::empty())
            .await
            .0,
        StatusCode::METHOD_NOT_ALLOWED
    );

    let mut unexpected_field = gstatic.clone();
    unexpected_field["url"] = json!("https://example.invalid");
    for bad in [
        json!({"generation": initial, "route": {"name": "Local"}, "check": "gstatic204", "timeout_ms": 2000}),
        unexpected_field,
        payload(initial, "Local", None, "https://example.invalid"),
        payload(initial, "Local\n", None, "gstatic204"),
        payload(
            initial,
            &"a".repeat(MAX_LABEL_BYTES + 1),
            None,
            "gstatic204",
        ),
    ] {
        assert_eq!(check_route(&router, &bad).await.0, StatusCode::BAD_REQUEST);
    }
    let mut too_long = gstatic.clone();
    too_long["timeout_ms"] = json!(MAX_TIMEOUT_MS + 1);
    assert_eq!(
        check_route(&router, &too_long).await.0,
        StatusCode::BAD_REQUEST
    );
    let huge = format!("{gstatic}{}", " ".repeat(MAX_REQUEST_BYTES));
    assert_eq!(
        send(&router, "POST", ROUTE, Some(SECRET), Body::from(huge))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    for (name, provider) in [
        ("Local", Some("Unknown")),
        ("Unknown", None),
        ("Choice", None),
        ("DIRECT", None),
        ("Direct alias", None),
        ("Chained", None),
        ("Duplicate", None),
        ("Duplicate", Some(&provider.name)),
    ] {
        assert_eq!(
            check_route(&router, &payload(initial, name, provider, "gstatic204"))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert!(local_dials.try_recv().is_err());
    assert!(remote_dials.try_recv().is_err());
    assert!(direct_dials.try_recv().is_err());
    assert!(chained_dials.try_recv().is_err());
    assert!(duplicate_dials.try_recv().is_err());

    local.health.record_delay(17);
    let history = local.last_delay();
    let selection = group.current();
    let stats = state.tunnel.statistics();
    let traffic = stats.snapshot();
    for (mock, dials, data, check, wire, expected) in [
        (
            &local,
            &mut local_dials,
            &gstatic,
            Check::Gstatic204,
            UPSTREAM_204,
            204,
        ),
        (
            &remote,
            &mut remote_dials,
            &cloudflare,
            Check::Cloudflare204,
            UPSTREAM_REDIRECT,
            307,
        ),
    ] {
        let server = attach_tls(mock, tls.clone(), check, wire);
        let (status, response) = check_route(&router, data).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            response,
            json!({"route": data["route"], "check": data["check"], "status": expected, "elapsed_ms": 0})
        );
        server.await.unwrap();
        let target = dials.try_recv().unwrap();
        assert_eq!(target.host, check.host());
        assert_eq!(target.dst_port, 443);
        assert_eq!(target.dst_ip, None);
        assert!(target.is_internal());
        assert!(dials.try_recv().is_err());
    }

    local.actions.lock().push_back(Action::Fail);
    assert_eq!(
        check_route(&router, &gstatic).await.0,
        StatusCode::BAD_GATEWAY
    );
    local_dials.try_recv().unwrap();
    assert!(local_dials.try_recv().is_err());
    assert!(local.alive());
    assert_eq!(local.last_delay(), history);
    assert_eq!(local.delay_history().len(), 1);
    assert!(remote.delay_history().is_empty());
    assert_eq!(group.current(), selection);
    assert_eq!(group.usage_generation(), 0);
    assert_eq!(stats.snapshot(), traffic);
    assert!(stats.active_connections().is_empty());

    let untrusted = controller_router(&state, &Arc::new(LoopbackController::new()), "[::1]:1080");
    let untrusted_check = payload(generation(&untrusted).await, "Local", None, "gstatic204");
    let (client, server) = duplex(16 * 1024);
    local.actions.lock().push_back(Action::Stream(client));
    let peer =
        tokio::spawn(async move { assert!(tokio_boring::accept(&tls, server).await.is_err()) });
    assert_eq!(
        check_route(&untrusted, &untrusted_check).await.0,
        StatusCode::BAD_GATEWAY
    );
    peer.await.unwrap();
    local_dials.try_recv().unwrap();
    assert!(local_dials.try_recv().is_err());

    let reserved = controller
        .admitted
        .acquire_many(u32::try_from(MAX_ADMITTED).unwrap())
        .await
        .unwrap();
    assert_eq!(
        check_route(&router, &gstatic).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(reserved);
    let held = controller
        .active
        .acquire_many(u32::try_from(MAX_ACTIVE).unwrap())
        .await
        .unwrap();
    let mut short = gstatic.clone();
    short["timeout_ms"] = json!(9);
    let start = Instant::now();
    assert_eq!(
        check_route(&router, &short).await.0,
        StatusCode::GATEWAY_TIMEOUT
    );
    assert_eq!(start.elapsed(), Duration::from_millis(9));
    drop(held);
    assert!(local_dials.try_recv().is_err());

    local.actions.lock().push_back(Action::Pending);
    let start = Instant::now();
    assert_eq!(
        check_route(&router, &short).await.0,
        StatusCode::GATEWAY_TIMEOUT
    );
    assert_eq!(start.elapsed(), Duration::from_millis(9));
    local_dials.try_recv().unwrap();
    assert!(local_dials.try_recv().is_err());

    let slow_body = Body::from_stream(futures::stream::pending::<Result<&str, std::io::Error>>());
    let start = Instant::now();
    assert_eq!(
        send(&router, "POST", ROUTE, Some(SECRET), slow_body)
            .await
            .0,
        StatusCode::GATEWAY_TIMEOUT
    );
    assert_eq!(
        start.elapsed(),
        Duration::from_millis(u64::from(MAX_TIMEOUT_MS))
    );

    tokio::time::resume();
    remote.actions.lock().push_back(Action::Pending);
    let waiting = tokio::spawn({
        let router = router.clone();
        let cloudflare = cloudflare.clone();
        async move { check_route(&router, &cloudflare).await.0 }
    });
    assert_eq!(
        remote_dials.recv().await.unwrap().host,
        Check::Cloudflare204.host()
    );
    provider.refresh().await.unwrap();
    assert_eq!(waiting.await.unwrap(), StatusCode::CONFLICT);
    assert_eq!(
        check_route(&router, &cloudflare).await.0,
        StatusCode::CONFLICT
    );
    assert!(remote_dials.try_recv().is_err());
    let refreshed = generation(&router).await;
    assert_ne!(initial, refreshed);

    // An admitted request queued before publication never reaches the new route.
    let held = controller
        .active
        .acquire_many(u32::try_from(MAX_ACTIVE).unwrap())
        .await
        .unwrap();
    let before_reload = payload(refreshed, "Local", None, "gstatic204");
    let queued = tokio::spawn({
        let router = router.clone();
        let before_reload = before_reload.clone();
        async move { check_route(&router, &before_reload).await.0 }
    });
    tokio::task::yield_now().await;
    state.tunnel.reload_routing(
        Default::default(),
        vec![],
        Default::default(),
        None,
        Default::default(),
    );
    drop(held);
    assert_eq!(queued.await.unwrap(), StatusCode::CONFLICT);
    assert_eq!(
        check_route(&router, &before_reload).await.0,
        StatusCode::CONFLICT
    );
    assert!(local_dials.try_recv().is_err());

    let next_session =
        controller_router(&state, &Arc::new(LoopbackController::new()), "[::1]:1080");
    let after_reload = generation(&router).await;
    let publishing = route_check::publishing();
    let (status, response) = send(&router, "GET", "/version", Some(SECRET), Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(response.get("capabilities").is_none());
    drop(publishing);
    let settled = generation(&router).await;
    assert_ne!(after_reload, settled);
    assert_eq!(
        check_route(
            &next_session,
            &payload(settled, "Local", None, "gstatic204")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn status_parser_stops_at_headers_and_enforces_total_bound() {
    for (response, expected) in [
        (
            b"HTTP/1.1 103 Early Hints\r\n\r\nHTTP/1.1 451 Denied\r\nContent-Length: 4\r\n\r\nbody"
                .as_slice(),
            Some(451),
        ),
        (b"HTTP/1.1 101 Switching Protocols\r\n\r\n".as_slice(), None),
        (b"HTTP/1.1 600 Invalid\r\n\r\n".as_slice(), None),
        (
            b"HTTP/1.1 204 No Content\r\nBad header: value\r\n\r\n".as_slice(),
            None,
        ),
    ] {
        let (mut client, mut server) = duplex(16 * 1024);
        server.write_all(response).await.unwrap();
        let actual = request_status(&mut client, Check::Gstatic204).await;
        assert_eq!(actual.ok(), expected);
        if expected.is_some() {
            let mut body = [0; 4];
            client.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"body");
        }
    }

    let prefix = "HTTP/1.1 103 Early Hints\r\n\r\nHTTP/1.1 204 No Content\r\nX-Fill: ";
    for size in [MAX_HEADER_BYTES, MAX_HEADER_BYTES + 1] {
        let (mut client, mut server) = duplex(16 * 1024);
        let response = format!("{prefix}{}\r\n\r\n", "x".repeat(size - prefix.len() - 4));
        server.write_all(response.as_bytes()).await.unwrap();
        let result = request_status(&mut client, Check::Gstatic204).await;
        assert_eq!(result.ok(), (size == MAX_HEADER_BYTES).then_some(204));
    }
}

#[tokio::test]
async fn route_check_does_not_reenter_a_proxy_for_dns() {
    use meow_common::HostResolver;

    let (proxy, mut dials) = MockProxy::new("recursive", AdapterType::Socks5, false);
    let proxy: Arc<dyn Proxy> = proxy;
    let resolver = meow_dns::Resolver::new_with_bootstrap_with_proxies(
        vec![meow_dns::NameServerEntry::parse("192.0.2.53#recursive").unwrap()],
        vec![],
        vec![],
        meow_common::DnsMode::Normal,
        meow_trie::DomainTrie::new(),
        true,
        false,
        None,
        None,
        &[("recursive".into(), proxy)].into_iter().collect(),
        true,
    )
    .await
    .unwrap();
    let cached = "192.0.2.2".parse().unwrap();
    resolver.preload_cache("cached.invalid", &[cached], Duration::from_secs(60));
    let hook = meow_dns::ResolverHostHook::new(Arc::new(resolver));
    assert_eq!(
        route_check::scope(hook.resolve("cached.invalid"))
            .await
            .unwrap(),
        cached
    );
    assert_eq!(
        route_check::scope(hook.resolve_all("uncached.invalid"))
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert!(dials.try_recv().is_err());
}
