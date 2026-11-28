//! Per-link systemd-resolved registration for the TUN's own DNS address.

use std::io;
use std::net::IpAddr;
use std::os::unix::fs::FileTypeExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RESOLVED_RUN: &str = "/run/systemd/resolve";
const COMMAND_TIMEOUT: Duration = Duration::from_millis(1500);

/// A managed stub may exist before resolved is ready. Never overwrite it,
/// even if the daemon is still starting or its D-Bus endpoint fails.
pub(super) fn manages_system_dns() -> bool {
    let file = Path::new("/etc/resolv.conf");
    let managed_file = std::fs::canonicalize(file).is_ok_and(|target| managed_target(&target))
        || std::fs::read_link(file)
            .is_ok_and(|target| managed_target(&Path::new("/etc").join(target)));
    let running = std::fs::metadata(Path::new(RESOLVED_RUN).join("io.systemd.Resolve"))
        .is_ok_and(|meta| meta.file_type().is_socket());
    managed_file || running
}

fn managed_target(link_target: &Path) -> bool {
    // Read-link can return a relative target, including while /run is
    // being populated. Recognize managed paths without following them.
    let mut path = PathBuf::new();
    for part in link_target.components() {
        if part == Component::ParentDir {
            path.pop();
        } else {
            path.push(part.as_os_str());
        }
    }
    path.starts_with(RESOLVED_RUN)
        || path == Path::new("/usr/lib/systemd/resolv.conf")
        || path == Path::new("/lib/systemd/resolv.conf")
}

pub(super) struct LinkDns {
    index: String,
    program: &'static str,
}

impl LinkDns {
    pub(super) fn install(if_index: u32, dns: IpAddr) -> io::Result<Self> {
        let program = ["/usr/bin/resolvectl", "/bin/resolvectl"]
            .into_iter()
            .find(|name| Path::new(name).is_file())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "resolvectl is unavailable"))?;
        let index = if_index.to_string();
        let dns = dns.to_string();
        configure(&index, &dns, |args| call(program, args))?;
        Ok(Self { index, program })
    }

    pub(super) fn restore(&self) -> io::Result<()> {
        call(self.program, &["revert", &self.index])
    }
}

fn configure(
    index: &str,
    dns: &str,
    mut run: impl FnMut(&[&str]) -> io::Result<()>,
) -> io::Result<()> {
    let result = (|| {
        run(&["dns", index, dns])?;
        run(&["domain", index, "~."])?;
        run(&["default-route", index, "yes"])
    })();
    if let Err(error) = result {
        // A timeout can follow a successful D-Bus write. Revert even if the
        // first command did not return success. This is only our TUN link.
        if let Err(rollback) = run(&["revert", index]) {
            tracing::warn!("tun dns-guard: resolved rollback of link {index} failed: {rollback}");
        }
        return Err(error);
    }
    Ok(())
}

fn call(program: &str, args: &[&str]) -> io::Result<()> {
    let mut child = Command::new(program)
        .env_clear()
        .env("LANG", "C")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(io::Error::other(format!(
                    "resolvectl {}: {status}",
                    args[0]
                )));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(15)),
            outcome => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(outcome.err().unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("resolvectl {} did not complete", args[0]),
                    )
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_paths_include_the_packaged_static_stub_and_relative_targets() {
        for managed in [
            "/run/systemd/resolve/stub-resolv.conf",
            "/etc/../run/systemd/resolve/resolv.conf",
            "/etc/../usr/lib/systemd/resolv.conf",
            "/lib/systemd/resolv.conf",
        ] {
            assert!(managed_target(Path::new(managed)), "{managed}");
        }
        for unmanaged in ["/etc/resolv.conf", "/run/systemd/resolve-other/resolv.conf"] {
            assert!(!managed_target(Path::new(unmanaged)), "{unmanaged}");
        }
    }

    #[test]
    fn registration_only_targets_the_tun_link() {
        let mut calls = Vec::new();
        configure("42", "198.18.0.2", |args| {
            calls.push(args.join(" "));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            calls,
            ["dns 42 198.18.0.2", "domain 42 ~.", "default-route 42 yes"]
        );
    }

    #[test]
    fn every_failed_step_reverts_only_its_own_link() {
        for step in 1..=3 {
            let mut calls = Vec::new();
            let error = configure("42", "198.18.0.2", |args| {
                calls.push(args.join(" "));
                if calls.len() == step {
                    Err(io::Error::new(io::ErrorKind::TimedOut, "injected"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert_eq!(calls.len(), step + 1);
            assert_eq!(calls.last().unwrap(), "revert 42");
        }
    }
}
