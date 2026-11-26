//! Offline synthetic classical-provider benchmark.
//!
//! Run: cargo bench -p meow-rules --bench classical_mixed_bench
//! Sizes count literal rules: runs of up to 256 alternating exact/suffix
//! domains, followed by keyword, regex, CIDR and port rules. The small case
//! has one 64-literal run; the large case has 128 256-literal runs.
//! All names use .test and addresses use documentation networks.
//!
//! Each lookup iteration visits 128 precomputed queries: 16 each of exact,
//! suffix, keyword, regex, CIDR, port, overlapping domain+port, and miss.
//! Domain positions cycle deterministically across the list. Construction
//! measures checked parsing, plan building AND destruction of the resulting
//! set; entries and parser context are prepared outside the timed loop.
//! The ordered reference checks only the provider's observable boolean,
//! not which internal rule matched first.
//!
//! The position comparison uses the large corpus in two layouts: separated
//! literal windows, or all literals followed by the same separators. Each
//! early/middle/late iteration checks one exact and one suffix hit; a miss
//! iteration checks one query. Layout and the number of rules visited both
//! change by design. Timings alone cannot isolate window overhead, establish
//! causation, or predict performance for real provider datasets.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use meow_common::{Metadata, Rule, RuleMatchHelper};
use meow_rules::{
    build_rule_set_checked, domain::DomainRule, domain_keyword::DomainKeywordRule,
    domain_regex::DomainRegexRule, domain_suffix::DomainSuffixRule, ipcidr::IpCidrRule,
    port::PortRule, ParserContext, RuleSetBehavior,
};

// Build both representations directly, in the same order. The reference uses
// individual Rule implementations and never uses the provider's parse/plan.
fn corpus(literals: usize) -> (Vec<String>, Vec<Box<dyn Rule>>) {
    let mut entries = Vec::new();
    let mut reference: Vec<Box<dyn Rule>> = Vec::new();
    for start in (0..literals).step_by(256) {
        for i in start..(start + 256).min(literals) {
            let host = format!("r{i}.bench.test");
            if i % 2 == 0 {
                entries.push(format!("DOMAIN,{host}"));
                reference.push(Box::new(DomainRule::new(&host, "DIRECT")));
            } else {
                entries.push(format!("DOMAIN-SUFFIX,{host}"));
                reference.push(Box::new(DomainSuffixRule::new(&host, "DIRECT")));
            }
        }
        entries.extend([
            "DOMAIN-KEYWORD,needle-bench".to_owned(),
            r"DOMAIN-REGEX,^regex[0-9]+\.bench\.test$".to_owned(),
            "IP-CIDR,192.0.2.0/24".to_owned(),
            "DST-PORT,4242".to_owned(),
        ]);
        reference.push(Box::new(DomainKeywordRule::new("needle-bench", "DIRECT")));
        reference.push(Box::new(
            DomainRegexRule::new(r"^regex[0-9]+\.bench\.test$", "DIRECT").unwrap(),
        ));
        reference.push(Box::new(
            IpCidrRule::new("192.0.2.0/24", "DIRECT", false, false).unwrap(),
        ));
        reference.push(Box::new(PortRule::new("4242", "DIRECT", false).unwrap()));
    }
    (entries, reference)
}

fn queries(literals: usize) -> Vec<(Metadata, bool)> {
    let mut result = Vec::with_capacity(128);
    for i in 0..16 {
        // A permutation of 16 positions from the beginning to the end.
        let position = (i * 7 % 16) * (literals - 2) / 15;
        let even = position & !1;
        let odd = even + 1;
        for (host, port, ip, expected) in [
            (format!("r{even}.bench.test"), 443, None, true),
            (format!("sub.r{odd}.bench.test"), 443, None, true),
            (format!("needle-bench{i}.test"), 443, None, true),
            (format!("regex{i}.bench.test"), 443, None, true),
            (format!("cidr{i}.bench.test"), 443, Some("192.0.2.1"), true),
            (format!("port{i}.bench.test"), 4242, None, true),
            (format!("r{even}.bench.test"), 4242, None, true),
            (
                format!("missing{i}.bench.test"),
                443,
                Some("203.0.113.1"),
                false,
            ),
        ] {
            result.push((
                Metadata {
                    host: host.into(),
                    dst_port: port,
                    dst_ip: ip.map(|s| s.parse().unwrap()),
                    ..Default::default()
                },
                expected,
            ));
        }
    }
    result
}

fn bench_classical_mixed(c: &mut Criterion) {
    let mut group = c.benchmark_group("classical_provider_mixed");
    for literals in [64usize, 32_768] {
        let (entries, reference) = corpus(literals);
        let ctx = ParserContext::empty();
        let set = build_rule_set_checked(RuleSetBehavior::Classical, &entries, &ctx).unwrap();
        assert_eq!(set.len(), entries.len());
        assert_eq!(reference.len(), entries.len());
        let queries = queries(literals);
        for (metadata, expected) in &queries {
            let ordered = reference
                .iter()
                .any(|rule| rule.match_metadata(metadata, &RuleMatchHelper));
            assert_eq!(ordered, *expected, "reference: {metadata:?}");
            assert_eq!(
                set.matches(metadata, &RuleMatchHelper),
                ordered,
                "provider: {metadata:?}"
            );
        }

        group.throughput(Throughput::Elements(entries.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("construct_and_drop", literals),
            &literals,
            |b, _| {
                b.iter(|| {
                    let built = build_rule_set_checked(
                        RuleSetBehavior::Classical,
                        black_box(&entries),
                        black_box(&ctx),
                    )
                    .unwrap();
                    drop(black_box(built));
                });
            },
        );
        group.throughput(Throughput::Elements(queries.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("lookup_mix", literals),
            &literals,
            |b, _| {
                b.iter(|| {
                    for (metadata, _) in &queries {
                        black_box(set.matches(black_box(metadata), &RuleMatchHelper));
                    }
                });
            },
        );
    }
    group.finish();
}

fn bench_classical_positions(c: &mut Criterion) {
    const LITERALS: usize = 32_768;
    let mut group = c.benchmark_group("classical_provider_positions");
    for layout in ["contiguous", "separated"] {
        let (entries, reference) = corpus(LITERALS);
        let (entries, reference) = if layout == "contiguous" {
            // Every complete block contains 256 literals and four separators.
            // Keep both representations in the same order as we move all
            // separators after the literals, without changing their counts.
            let (mut literals, separators): (Vec<_>, Vec<_>) = entries
                .into_iter()
                .zip(reference)
                .enumerate()
                .partition(|(index, _)| index % 260 < 256);
            literals.extend(separators);
            literals.into_iter().map(|(_, pair)| pair).unzip()
        } else {
            (entries, reference)
        };
        let set = build_rule_set_checked(
            RuleSetBehavior::Classical,
            &entries,
            &ParserContext::empty(),
        )
        .unwrap();
        assert_eq!(set.len(), entries.len());
        assert_eq!(reference.len(), entries.len());

        for (position, index) in [
            ("early", Some(0)),
            ("middle", Some(LITERALS / 2)),
            ("late", Some(LITERALS - 2)),
            ("miss", None),
        ] {
            let hosts = match index {
                Some(i) => vec![
                    format!("r{i}.bench.test"),
                    format!("sub.r{}.bench.test", i + 1),
                ],
                None => vec!["missing.bench.test".to_owned()],
            };
            let queries: Vec<_> = hosts
                .into_iter()
                .map(|host| Metadata {
                    host: host.into(),
                    dst_port: 443,
                    ..Default::default()
                })
                .collect();
            for metadata in &queries {
                let ordered = reference
                    .iter()
                    .any(|rule| rule.match_metadata(metadata, &RuleMatchHelper));
                assert_eq!(ordered, index.is_some(), "reference: {layout} {position}");
                assert_eq!(
                    set.matches(metadata, &RuleMatchHelper),
                    ordered,
                    "provider: {layout} {position}"
                );
            }

            group.throughput(Throughput::Elements(queries.len() as u64));
            group.bench_function(BenchmarkId::new(layout, position), |b| {
                b.iter(|| {
                    for metadata in &queries {
                        black_box(set.matches(black_box(metadata), &RuleMatchHelper));
                    }
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_classical_mixed, bench_classical_positions);
criterion_main!(benches);
