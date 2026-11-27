use super::*;
use std::net::Ipv4Addr;

const DOMESTIC: [u8; 4] = [192, 0, 2, 19];
const FOREIGN: [u8; 4] = [198, 51, 100, 23];
const NO_RECORD: [u8; 4] = [203, 0, 113, 29];
const BACKUP: [u8; 4] = [203, 0, 113, 177];

// Original tiny, deterministic MaxMind DB fixture: 24-bit binary search
// nodes, record pointers into two data records, and complete v2 metadata.
// No shipped/vendor database files or product home needed.
fn fixture(country_records: bool) -> Vec<u8> {
    fn string(out: &mut Vec<u8>, content: &str) {
        assert!(content.len() < 29);
        out.push(0x40 | content.len() as u8);
        out.extend_from_slice(content.as_bytes());
    }
    fn country(bytes: &mut Vec<u8>, code: &str, country_records: bool) {
        bytes.push(0xe1);
        if country_records {
            string(bytes, "country");
            bytes.push(0xe1);
            string(bytes, "iso_code");
            string(bytes, code);
        } else {
            string(bytes, "autonomous_system_number");
            bytes.extend_from_slice(&[0xc1, 42]);
        }
    }
    let mut tree: Vec<[Option<usize>; 2]> = vec![[None; 2]];
    let mut leaf_edges = Vec::new();
    for ip in [DOMESTIC, FOREIGN] {
        let mut node = 0;
        let address = u32::from_be_bytes(ip);
        for bit in (1..32).rev() {
            let edge = (address >> bit & 1) as usize;
            let next = if let Some(next) = tree[node][edge] {
                next
            } else {
                let next = tree.len();
                tree[node][edge] = Some(next);
                tree.push([None; 2]);
                next
            };
            node = next;
        }
        leaf_edges.push((node, (address & 1) as usize));
    }
    let nodes = tree.len();
    let mut data = Vec::new();
    for ((node, edge), code) in leaf_edges.into_iter().zip(["DE", "US"]) {
        tree[node][edge] = Some(nodes + 16 + data.len());
        country(&mut data, code, country_records);
    }
    let mut bytes = Vec::new();
    for edges in tree {
        for value in edges {
            bytes.extend_from_slice(&(value.unwrap_or(nodes) as u32).to_be_bytes()[1..]);
        }
    }
    bytes.extend([0; 16]);
    bytes.extend(data);
    bytes.extend(b"\xAB\xCD\xEFMaxMind.com");
    bytes.push(0xe9);
    string(&mut bytes, "node_count");
    bytes.extend([0xc1, nodes as u8]);
    string(&mut bytes, "record_size");
    bytes.extend([0xa1, 24]);
    string(&mut bytes, "ip_version");
    bytes.extend([0xa1, 4]);
    string(&mut bytes, "database_type");
    string(&mut bytes, "Synthetic-Country");
    string(&mut bytes, "languages");
    bytes.extend([1, 4]); // Extended type 11 (array), one member.
    string(&mut bytes, "en");
    string(&mut bytes, "binary_format_major_version");
    bytes.extend([0xa1, 2]);
    string(&mut bytes, "binary_format_minor_version");
    bytes.push(0xa0);
    string(&mut bytes, "build_epoch");
    bytes.extend([1, 2, 1]); // Extended type 9 (uint64), one byte.
    string(&mut bytes, "description");
    bytes.push(0xe1);
    string(&mut bytes, "en");
    string(&mut bytes, "Synthetic test data");
    bytes
}

#[test]
fn prepared_fallback_geoip_owns_verified_reader_and_preserves_unknown_addresses() {
    assert!(PreparedFallbackGeoIp::from_bytes(vec![0; 36]).is_err());
    assert!(PreparedFallbackGeoIp::from_bytes(fixture(false)).is_err());
    let prepared = PreparedFallbackGeoIp::from_bytes(fixture(true)).unwrap();
    let filter = crate::raw::RawFallbackFilter {
        geoip: Some(true),
        geoip_code: Some("DE".into()),
        ..Default::default()
    };
    let local = tempfile::tempdir().unwrap();
    let db = local.path().join("Country.mmdb");
    // No reader can be opened from this path; the exact prepared Arc wins.
    let compiled =
        build_fallback_filter_inner(Some(&filter), Some(&db), false, Some(prepared.clone()))
            .unwrap();
    assert!(Arc::ptr_eq(
        compiled.geoip_reader.as_ref().unwrap(),
        &prepared.0
    ));
    let ip = |bytes| IpAddr::from(Ipv4Addr::from(bytes));
    assert!(!compiled.ip_gated(&[ip(DOMESTIC)]));
    assert!(compiled.ip_gated(&[ip(FOREIGN)]));
    assert!(!compiled.ip_gated(&[ip(NO_RECORD)]));
    let legacy = build_fallback_filter(Some(&filter), Some(&db), true).unwrap();
    assert!(!legacy.geoip_enabled);
    assert!(!legacy.ip_gated(&[ip(FOREIGN)]));
}

#[tokio::test]
async fn prepared_fallback_geoip_selects_real_answers_without_provisioning() {
    fn answer(query: &[u8], ip: [u8; 4]) -> Vec<u8> {
        let mut end = 12;
        while query[end] != 0 {
            end += 1 + usize::from(query[end]);
        }
        end += 5;
        let mut reply = query[..end].to_vec();
        reply[2..4].copy_from_slice(&[0x81, 0x80]);
        reply[6..12].copy_from_slice(&[0, 1, 0, 0, 0, 0]);
        reply.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        reply.extend_from_slice(&ip);
        reply
    }
    async fn serve(socket: tokio::net::UdpSocket, is_fallback: bool) {
        let mut buffer = [0; 1536];
        loop {
            let (n, peer) = socket.recv_from(&mut buffer).await.unwrap();
            let ip = if is_fallback {
                BACKUP
            } else {
                match buffer[13] {
                    b'd' => DOMESTIC,
                    b'f' => FOREIGN,
                    _ => NO_RECORD,
                }
            };
            socket
                .send_to(&answer(&buffer[..n], ip), peer)
                .await
                .unwrap();
        }
    }
    let primary = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let fallback = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = tempfile::tempdir().unwrap();
    let db = local.path().join("Country.mmdb");
    let yaml = format!(
        "ipv6: false\ngeodata:\n  mmdb-path: '{}'\ndns:\n  enable: true\n  use-system-hosts: false\n  nameserver: ['udp://{}']\n  fallback: ['udp://{}']\n  fallback-filter: {{geoip: true, geoip-code: DE}}\nrules: ['MATCH,DIRECT']\n",
        db.display(),
        primary.local_addr().unwrap(),
        fallback.local_addr().unwrap()
    );
    let primary_task = tokio::spawn(serve(primary, false));
    let fallback_task = tokio::spawn(serve(fallback, true));
    let prepared = PreparedFallbackGeoIp::from_bytes(fixture(true)).unwrap();
    let mut raw = crate::parse_raw_yaml(&yaml).unwrap();
    let fallback_pool = raw.dns.as_mut().unwrap().fallback.take();
    assert!(
        crate::build_config_with_fallback_geoip(raw.clone(), None, &prepared)
            .await
            .is_err()
    );
    raw.dns.as_mut().unwrap().fallback = fallback_pool;
    let config = crate::build_config_with_fallback_geoip(raw.clone(), None, &prepared)
        .await
        .unwrap();
    let rebuilt = crate::parse_dns_from_raw_with_fallback_geoip(
        &raw,
        None,
        &HashMap::new(),
        None,
        None,
        Some(&config.dns.resolver),
        None,
        Some(&prepared),
    )
    .await
    .unwrap();
    assert!(
        !db.exists(),
        "prepared fallback cannot provision a missing file"
    );
    for (host, expected) in [
        ("domestic", DOMESTIC),
        ("foreign", BACKUP),
        ("unknown", NO_RECORD),
    ] {
        let mut query = vec![0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, host.len() as u8];
        query.extend_from_slice(host.as_bytes());
        query.extend_from_slice(b"\x04test\x00\x00\x01\x00\x01");
        for resolver in [&config.dns.resolver, &rebuilt.resolver] {
            let reply = meow_dns::DnsServer::handle_query(&query, resolver)
                .await
                .unwrap();
            assert_eq!(&reply[6..8], &[0, 1], "{host}");
            assert_eq!(&reply[reply.len() - 4..], &expected, "{host}");
        }
    }
    primary_task.abort();
    fallback_task.abort();
}
