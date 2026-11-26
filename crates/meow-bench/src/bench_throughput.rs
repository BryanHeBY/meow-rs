use anyhow::Context;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::socks5_client::socks5_connect;

/// Stall bound on post-connect IO: a datapath that accepts but never
/// echoes must fail the leg, not hang the suite.  Generous — a healthy
/// 64 MB loopback transfer finishes in well under a second.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);
/// Per-round-trip bound for the small-message loop.
const MSG_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, serde::Serialize)]
pub struct ThroughputResult {
    pub label: String,
    pub total_bytes: u64,
    pub elapsed_secs: f64,
    pub gbps: f64,
}

/// Exchange one payload without detached tasks: on timeout or failure, both
/// halves and the connection are dropped. The deadline covers writes as well
/// as reads, so a stalled writer cannot hang after a reader finishes.
async fn exchange_large(
    stream: tokio::net::TcpStream,
    size: usize,
    timeout: Duration,
) -> anyhow::Result<u64> {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let exchange = async {
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move {
            let chunk = vec![0xABu8; 64 * 1024];
            let mut remaining = size;
            while remaining > 0 {
                let n = remaining.min(chunk.len());
                wr.write_all(&chunk[..n]).await?;
                remaining -= n;
            }
            Ok::<_, anyhow::Error>(())
        });
        tasks.spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut received = 0usize;
            while received < size {
                let n = rd
                    .read(&mut buf[..(size - received).min(64 * 1024)])
                    .await?;
                anyhow::ensure!(
                    n != 0,
                    "large transfer ended early: received {received} of {size} echo bytes"
                );
                anyhow::ensure!(
                    buf[..n].iter().all(|&byte| byte == 0xAB),
                    "large transfer echo content mismatch at or after byte {received}"
                );
                received += n;
            }
            Ok::<_, anyhow::Error>(())
        });
        while let Some(result) = tasks.join_next().await {
            result??;
        }
        Ok::<_, anyhow::Error>((size as u64) * 2)
    };
    tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| anyhow::anyhow!("large transfer stalled after {timeout:?}"))?
}

/// Large transfer: write `size` bytes, read them back, using concurrent read+write.
async fn run_large_transfer(
    proxy: SocketAddr,
    echo: SocketAddr,
    size: usize,
) -> anyhow::Result<(u64, f64)> {
    let stream = socks5_connect(proxy, echo)
        .await
        .with_context(|| format!("large transfer connect to {echo}"))?;
    let start = Instant::now();
    let total = exchange_large(stream, size, TRANSFER_TIMEOUT).await?;
    Ok((total, start.elapsed().as_secs_f64()))
}

/// Small-message test: single connection, many round-trips of `msg_size` bytes.
async fn run_small_messages(
    proxy: SocketAddr,
    echo: SocketAddr,
    msg_size: usize,
    count: usize,
) -> anyhow::Result<(u64, f64)> {
    let mut stream = socks5_connect(proxy, echo)
        .await
        .with_context(|| format!("small messages connect to {echo}"))?;
    let msg = vec![0xABu8; msg_size];
    let mut buf = vec![0u8; msg_size];
    let mut total_bytes = 0u64;

    let start = Instant::now();
    for _ in 0..count {
        tokio::time::timeout(MSG_TIMEOUT, async {
            stream.write_all(&msg).await?;
            stream.read_exact(&mut buf).await?;
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("small message round-trip stalled after {MSG_TIMEOUT:?}"))??;
        anyhow::ensure!(buf == msg, "small message echo content mismatch");
        total_bytes += (msg_size * 2) as u64;
    }
    let elapsed = start.elapsed().as_secs_f64();

    Ok((total_bytes, elapsed))
}

pub async fn bench_throughput(
    proxy: SocketAddr,
    echo: SocketAddr,
) -> anyhow::Result<Vec<ThroughputResult>> {
    let mut results = Vec::new();

    // Small messages: single connection, many round-trips
    {
        let label = "4 KB x 10000";
        let (total_bytes, elapsed) = run_small_messages(proxy, echo, 4 * 1024, 10_000).await?;
        let gbps = (total_bytes as f64 * 8.0) / elapsed / 1e9;
        eprintln!("  throughput {label}: {gbps:.2} Gbps");
        results.push(ThroughputResult {
            label: label.to_string(),
            total_bytes,
            elapsed_secs: elapsed,
            gbps,
        });
    }

    // Medium transfers: multiple connections
    {
        let label = "1 MB x 10";
        let mut total_bytes = 0u64;
        let mut total_elapsed = 0.0f64;
        for _ in 0..10 {
            let (bytes, elapsed) = run_large_transfer(proxy, echo, 1024 * 1024).await?;
            total_bytes += bytes;
            total_elapsed += elapsed;
        }
        let gbps = (total_bytes as f64 * 8.0) / total_elapsed / 1e9;
        eprintln!("  throughput {label}: {gbps:.2} Gbps");
        results.push(ThroughputResult {
            label: label.to_string(),
            total_bytes,
            elapsed_secs: total_elapsed,
            gbps,
        });
    }

    // Large single transfer
    {
        let label = "64 MB x 1";
        let (total_bytes, elapsed) = run_large_transfer(proxy, echo, 64 * 1024 * 1024).await?;
        let gbps = (total_bytes as f64 * 8.0) / elapsed / 1e9;
        eprintln!("  throughput {label}: {gbps:.2} Gbps");
        results.push(ThroughputResult {
            label: label.to_string(),
            total_bytes,
            elapsed_secs: elapsed,
            gbps,
        });
    }

    // Four simultaneous 16 MiB streams; connection setup is outside the
    // wall-clock interval, as it is for the existing large transfer cases.
    {
        const FLOWS: usize = 4;
        const SIZE: usize = 16 * 1024 * 1024;
        let mut streams = Vec::with_capacity(FLOWS);
        for _ in 0..FLOWS {
            streams.push(socks5_connect(proxy, echo).await?);
        }
        let start = Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        for stream in streams {
            tasks.spawn(exchange_large(stream, SIZE, TRANSFER_TIMEOUT));
        }
        let mut total_bytes = 0u64;
        while let Some(result) = tasks.join_next().await {
            total_bytes += result??;
        }
        let elapsed = start.elapsed().as_secs_f64();
        let gbps = (total_bytes as f64 * 8.0) / elapsed / 1e9;
        let label = "16 MB x 4 concurrent";
        eprintln!("  throughput {label}: {gbps:.2} Gbps");
        results.push(ThroughputResult {
            label: label.to_string(),
            total_bytes,
            elapsed_secs: elapsed,
            gbps,
        });
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn truncated_echo_is_not_a_throughput_result() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            stream.write_all(&[5, 0]).await.unwrap();
            let mut request = [0; 10];
            stream.read_exact(&mut request).await.unwrap();
            stream
                .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            let mut payload = [0; 4096];
            stream.read_exact(&mut payload).await.unwrap();
            // Accept the entire upload but return only half of its echo.
            stream.write_all(&payload[..2048]).await.unwrap();
        });
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            run_large_transfer(proxy, "127.0.0.1:9".parse().unwrap(), 4096),
        )
        .await
        .expect("truncated echo should finish promptly")
        .expect_err("an incomplete transfer must not produce a throughput score");
        assert!(error.to_string().contains("received 2048 of 4096"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn corrupted_echo_is_not_a_throughput_result() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut payload = [0; 4096];
            stream.read_exact(&mut payload).await.unwrap();
            payload[2048] ^= 1;
            stream.write_all(&payload).await.unwrap();
        });
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let error = exchange_large(stream, 4096, Duration::from_secs(5))
            .await
            .expect_err("corrupted echo must not produce a score");
        assert!(error.to_string().contains("content mismatch"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_transfer_times_out_and_closes_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            loop {
                if stream.read(&mut buf).await.unwrap() == 0 {
                    break;
                }
            }
        });
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let error = exchange_large(stream, 4096, Duration::from_millis(100))
            .await
            .expect_err("stalled echo must not produce a score");
        assert!(error.to_string().contains("stalled"));
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("timeout must close the socket")
            .unwrap();
    }
}
