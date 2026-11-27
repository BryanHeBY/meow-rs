pub const DOMESTIC: [u8; 4] = [192, 0, 2, 19];
pub const FOREIGN: [u8; 4] = [198, 51, 100, 23];

// Original tiny, deterministic MaxMind DB fixture: 24-bit binary search
// nodes, record pointers into two data records, and complete v2 metadata.
// No shipped/vendor database files or product home needed.
pub fn fixture(country_records: bool) -> Vec<u8> {
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

