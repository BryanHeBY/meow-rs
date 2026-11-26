//! Offline profile comparison through already-running localhost SOCKS listeners.
//! The matching PIDs are sampled on Linux; no external server or network is used.

#[path = "../src/bench_throughput.rs"]
mod bench_throughput;
#[path = "../src/echo_server.rs"]
mod echo_server;
#[allow(dead_code)]
#[path = "../src/socks5_client.rs"]
mod socks5_client;

use anyhow::{ensure, Context};
use clap::Parser;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Parser)]
struct Args {
    /// Two or three localhost SOCKS addresses in variant order.
    #[arg(required = true, num_args = 2..=3)]
    proxies: Vec<SocketAddr>,
    /// Matching proxy process IDs in the same order, comma separated.
    #[arg(long, value_delimiter = ',', required = true)]
    pids: Vec<u32>,
    /// Multiples of six balance all orders for two or three variants.
    #[arg(long, default_value_t = 6, value_parser = clap::value_parser!(u32).range(1..))]
    rounds: u32,
    /// New SOCKS connections with a one-byte echo in each sample.
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..))]
    connections: u32,
}

#[derive(Clone, serde::Serialize)]
struct ProcessSnapshot {
    cpu_ticks: u64,
    rss_kib: u64,
    high_water_kib: u64,
}

// Only sample the proxy itself (not the benchmark client/echo server).
// /proc/stat's comm may contain spaces and parentheses; find the last ')'.
fn process_snapshot(pid: u32) -> anyhow::Result<ProcessSnapshot> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let end = stat.rfind(')').context("invalid process stat")?;
    let fields: Vec<_> = stat[end + 1..].split_whitespace().collect();
    ensure!(fields.len() > 12, "short process stat for PID {pid}");
    let cpu_ticks = fields[11].parse::<u64>()? + fields[12].parse::<u64>()?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let value = |name: &str| -> anyhow::Result<u64> {
        let line = status
            .lines()
            .find(|line| line.starts_with(name))
            .with_context(|| format!("missing {name} for PID {pid}"))?;
        Ok(line
            .split_whitespace()
            .nth(1)
            .context("missing memory value")?
            .parse()?)
    };
    Ok(ProcessSnapshot {
        cpu_ticks,
        rss_kib: value("VmRSS:")?,
        high_water_kib: value("VmHWM:")?,
    })
}

async fn short_connections(
    proxy: SocketAddr,
    echo: SocketAddr,
    count: u32,
) -> anyhow::Result<serde_json::Value> {
    let mut latencies = Vec::with_capacity(count as usize);
    let start = Instant::now();
    for _ in 0..count {
        let begin = Instant::now();
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut stream = socks5_client::socks5_connect(proxy, echo).await?;
            stream.write_all(&[0x42]).await?;
            let mut received = [0u8; 1];
            stream.read_exact(&mut received).await?;
            anyhow::ensure!(received == [0x42], "short echo content mismatch");
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("short connection timed out")??;
        latencies.push(begin.elapsed().as_secs_f64() * 1e6);
    }
    let seconds = start.elapsed().as_secs_f64();
    latencies.sort_by(f64::total_cmp);
    let percentile = |p: f64| latencies[((latencies.len() - 1) as f64 * p).round() as usize];
    Ok(
        serde_json::json!({"connections": count, "elapsed_secs": seconds,
        "connections_per_sec": count as f64 / seconds, "p50_us": percentile(0.50),
        "p95_us": percentile(0.95), "p99_us": percentile(0.99),
        "min_us": latencies[0], "max_us": latencies[latencies.len()-1]}),
    )
}

fn cpu_measurement(
    before: &ProcessSnapshot,
    after: &ProcessSnapshot,
    wall: f64,
    hz: f64,
) -> anyhow::Result<serde_json::Value> {
    let ticks = after
        .cpu_ticks
        .checked_sub(before.cpu_ticks)
        .context("process CPU counter went backwards")?;
    let seconds = ticks as f64 / hz;
    Ok(serde_json::json!({"wall_secs": wall, "cpu_secs": seconds,
        "cpu_percent_one_core": seconds / wall * 100.0, "before": before, "after": after}))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    ensure!(
        args.proxies.len() == args.pids.len(),
        "one PID is required per proxy"
    );
    ensure!(
        args.proxies.iter().all(|addr| addr.ip().is_loopback()),
        "proxies must bind loopback"
    );
    ensure!(
        args.pids.iter().all(|&pid| pid > 0),
        "PIDs must be positive"
    );
    let hz_output = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()?;
    ensure!(hz_output.status.success(), "getconf CLK_TCK failed");
    let hz: f64 = std::str::from_utf8(&hz_output.stdout)?.trim().parse()?;
    ensure!(hz > 0.0, "invalid clock tick rate");
    let orders: &[&[usize]] = if args.proxies.len() == 2 {
        &[&[0, 1], &[1, 0]]
    } else {
        &[
            &[0, 1, 2],
            &[0, 2, 1],
            &[1, 0, 2],
            &[1, 2, 0],
            &[2, 0, 1],
            &[2, 1, 0],
        ]
    };
    let (echo, server) = echo_server::start_echo_server().await?;
    for &proxy in &args.proxies {
        short_connections(proxy, echo, 10).await?;
        bench_throughput::bench_throughput(proxy, echo).await?;
    }
    let mut samples = Vec::new();
    for round in 0..args.rounds {
        for &variant in orders[round as usize % orders.len()] {
            let pid = args.pids[variant];
            let before = process_snapshot(pid)?;
            let start = Instant::now();
            let short = short_connections(args.proxies[variant], echo, args.connections).await?;
            let wall = start.elapsed().as_secs_f64();
            let after = process_snapshot(pid)?;
            let short_process = cpu_measurement(&before, &after, wall, hz)?;
            let before = after;
            let start = Instant::now();
            let throughput =
                bench_throughput::bench_throughput(args.proxies[variant], echo).await?;
            let wall = start.elapsed().as_secs_f64();
            let after = process_snapshot(pid)?;
            let throughput_process = cpu_measurement(&before, &after, wall, hz)?;
            samples.push(serde_json::json!({"round": round, "variant": variant,
                "short": short, "short_process": short_process,
                "throughput": throughput, "throughput_process": throughput_process}));
        }
        eprintln!("completed round {}/{}, all proxies", round + 1, args.rounds);
    }
    server.abort();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "clock_ticks_per_sec": hz, "proxies": args.proxies.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "pids": args.pids, "rounds": args.rounds,
        "connections_per_sample": args.connections, "samples": samples
        }))?
    );
    Ok(())
}
