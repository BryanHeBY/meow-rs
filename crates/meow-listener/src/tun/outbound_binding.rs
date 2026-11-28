//! Outbound-interface binding for global route scope (#375, issue #695).
//!
//! Global route scope (`auto-route: true`, or `global`) steers all traffic
//! into the TUN, so every socket
//! meow opens must be bound to the physical interface (via
//! `meow_common::install_outbound_interface`: `SO_BINDTODEVICE` on Linux,
//! `IP_BOUND_IF` on macOS, `IP_UNICAST_IF` on Windows) or it loops back
//! into the device.
//! The binding is per-socket and applied at creation: a socket created
//! before it is installed stays unbound for its whole life, and once the
//! routes into the device go in its traffic re-enters the TUN — a QUIC or mux
//! session opened by an early startup dial (geodata download, provider
//! fetch, health probe) then loops until it times out.
//!
//! [`OutboundBinding`] owns one installation of that process-global
//! binding. The binary installs it *before* the config build performs its
//! first dial, and a config reload installs the new configuration's
//! binding before the reload's first dial (both via
//! `meow_api::preinstall_global_route_binding`); either hands it to
//! [`TunListener::with_outbound_binding`]. A listener started without one
//! (the pre-install failed) installs its own before touching routes.
//! Either way the listener's device owns it, so it is given up exactly
//! when the routes it protects go away — listener teardown, reload, or a
//! startup failure.
//!
//! Owners may overlap — a reload's binding is installed while the old
//! listener still runs. The registry is owner-aware
//! (`meow_common::OutboundIfaceGuard`): the newest live owner is in
//! effect, dropping it restores the newest owner still alive, and dropping
//! a superseded owner is a no-op. So a rejected reload restores the
//! running listener's binding, and an old listener torn down after its
//! successor installed never clears the successor's binding.
//!
//! [`TunListener::with_outbound_binding`]: super::TunListener::with_outbound_binding

use std::io;

use tracing::info;

/// One owner of the process-global outbound-interface binding; dropping it
/// gives the binding up (the newest owner still alive takes over, or it is
/// cleared when none is left).
#[must_use = "dropping the binding gives it up immediately"]
#[derive(Debug)]
pub struct OutboundBinding(meow_common::OutboundIfaceGuard);

impl OutboundBinding {
    /// Install the binding for global route scope. `outbound_interface` is
    /// `tun.outbound-interface`; `None` auto-detects the interface carrying
    /// the IPv4 default route (`0.0.0.0/0` — the TUN's own split `/1`
    /// routes are skipped, so detection is safe while a global-scope
    /// listener is still running).
    ///
    /// Fails closed: an error means nothing is installed, and callers must
    /// not install global routes. Targets other than Linux, macOS and
    /// Windows always fail — they have no per-socket binding (#375).
    pub fn install(outbound_interface: Option<&str>) -> io::Result<Self> {
        if !cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows"
        )) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "tun auto-route: global scope is only implemented on Linux, macOS and \
                 Windows (#375); use auto-route: fake-ip on this platform",
            ));
        }
        let iface = match outbound_interface {
            Some(name) => name.to_owned(),
            None => super::route::default_interface().map_err(|e| {
                io::Error::other(format!(
                    "tun auto-route (global scope): could not auto-detect the physical \
                     interface ({e}); set tun.outbound-interface explicitly"
                ))
            })?,
        };
        Self::install_interface(&iface)
    }

    /// Wait only for Linux's automatically detected IPv4 default route.
    /// An unreadable route table, an invalid explicit interface or a bind
    /// failure is returned immediately. Dropping the future cancels the
    /// wait; it owns no task and changes no routing state while waiting.
    #[cfg(target_os = "linux")]
    pub async fn install_for_startup(outbound_interface: Option<&str>) -> io::Result<Self> {
        if let Some(name) = outbound_interface {
            return Self::install_interface(name);
        }
        let iface = wait_for_linux_default(super::route::linux_default_interface).await?;
        Self::install_interface(&iface)
    }

    fn install_interface(iface: &str) -> io::Result<Self> {
        let guard = meow_common::install_outbound_interface(iface).map_err(|e| {
            io::Error::other(format!(
                "tun auto-route (global scope): outbound interface binding failed ({e}); \
                 refusing to install default routes without loop avoidance"
            ))
        })?;
        info!("tun: global route scope — outbound sockets bound to '{iface}' (#375)");
        Ok(Self(guard))
    }

    /// The interface this binding installed.
    pub fn interface(&self) -> &str {
        self.0.interface()
    }

    /// Mark the binding's listener as up: from here the logbook records
    /// it (`meow_common::OutboundIfaceGuard::commit`). A reload candidate
    /// rejected before its listener starts never gets here.
    pub fn commit(&self) {
        self.0.commit();
    }
}

// Startup can race network-manager/DHCP after boot or reconnect. Keep
// the window short and scoped to startup, never an API config transaction.
#[cfg(target_os = "linux")]
async fn wait_for_linux_default(
    mut read: impl FnMut() -> io::Result<Option<String>>,
) -> io::Result<String> {
    use std::time::Duration;
    use tokio::time::{sleep_until, Instant};

    const LIMIT: Duration = Duration::from_secs(10);
    const TICK: Duration = Duration::from_millis(250);
    let deadline = Instant::now() + LIMIT;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no physical IPv4 default route appeared within 10s",
            ));
        }
        if let Some(iface) = read()? {
            return Ok(iface);
        }
        sleep_until((now + TICK).min(deadline)).await;
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::OutboundBinding;
    use crate::tun::{TunListener, TunListenerConfig, TunRouteScope, TunStack};

    /// Loopback exists on every host.
    const LO: &str = if cfg!(target_os = "macos") {
        "lo0"
    } else {
        "lo"
    };

    /// One test drives every case because the registry is process-global;
    /// separate `#[test]` fns would race each other.
    #[test]
    fn binding_installs_fails_closed_and_composes_across_owners() {
        assert!(meow_common::outbound_interface().is_none());

        // A missing interface fails closed and installs nothing.
        assert!(OutboundBinding::install(Some("no-such-iface-zz9")).is_err());
        assert!(meow_common::outbound_interface().is_none());

        let binding = OutboundBinding::install(Some(LO)).expect("loopback must exist");
        assert_eq!(binding.interface(), LO);
        assert_eq!(meow_common::outbound_interface().as_deref(), Some(LO));
        drop(binding);
        assert!(meow_common::outbound_interface().is_none());

        // A binding handed to a listener is owned by it: dropping a
        // listener that never ran (startup aborted before the TUN came up)
        // clears the binding instead of leaking it into a TUN-less process.
        let listener = global_listener(OutboundBinding::install(Some(LO)).unwrap());
        assert_eq!(meow_common::outbound_interface().as_deref(), Some(LO));
        drop(listener);
        assert!(meow_common::outbound_interface().is_none());

        // Reload restart (issue #695): the new config's binding is
        // installed while the old listener still owns its own; tearing the
        // old listener down afterwards must leave the new binding in
        // place (a single-slot registry cleared it here).
        let old = global_listener(OutboundBinding::install(Some(LO)).unwrap());
        let reload = OutboundBinding::install(Some(LO)).expect("loopback must exist");
        drop(old);
        assert_eq!(meow_common::outbound_interface().as_deref(), Some(LO));
        let new = global_listener(reload);
        assert_eq!(meow_common::outbound_interface().as_deref(), Some(LO));
        drop(new);
        assert!(meow_common::outbound_interface().is_none());

        // Rejected reload: its binding goes away first, and the running
        // listener's binding is back in effect rather than cleared.
        let running = global_listener(OutboundBinding::install(Some(LO)).unwrap());
        let rejected = OutboundBinding::install(Some(LO)).expect("loopback must exist");
        drop(rejected);
        assert_eq!(meow_common::outbound_interface().as_deref(), Some(LO));
        drop(running);
        assert!(meow_common::outbound_interface().is_none());
    }

    fn global_listener(binding: OutboundBinding) -> TunListener {
        TunListener::new(
            crate::test_rule_tunnel(),
            TunListenerConfig {
                device: None,
                mtu: 1500,
                inet4_address: "172.19.0.1/30".parse().unwrap(),
                inet6_address: None,
                auto_route: true,
                route_scope: TunRouteScope::Global,
                outbound_interface: Some(LO.into()),
                dns_hijack: false,
                udp_timeout: std::time::Duration::from_secs(60),
                max_connections: 0,
                stack: TunStack::Lwip,
            },
            "meow-tun-test".into(),
        )
        .with_outbound_binding(binding)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod startup_tests {
    use super::{wait_for_linux_default, OutboundBinding};
    use std::{cell::Cell, io, time::Duration};
    use tokio::time::{advance, sleep, Instant};

    #[tokio::test(start_paused = true)]
    async fn startup_waits_for_dhcp_without_opening_an_unbound_path() {
        let start = Instant::now();
        let bound = wait_for_linux_default(|| {
            Ok((start.elapsed() >= Duration::from_secs(4)).then(|| "eth-test".into()))
        })
        .await
        .unwrap();
        assert_eq!(bound, "eth-test");
        assert_eq!(start.elapsed(), Duration::from_secs(4));
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_and_permanent_errors_are_distinct() {
        let start = Instant::now();
        let err = wait_for_linux_default(|| Ok(None)).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(start.elapsed(), Duration::from_secs(10));

        // NotFound from an unreadable /proc/net/route is different from a
        // successful read that found no matching route.
        let start = Instant::now();
        let err = wait_for_linux_default(|| Err(io::ErrorKind::NotFound.into()))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(start.elapsed(), Duration::ZERO);

        assert!(
            OutboundBinding::install_for_startup(Some("no-such-iface-test"))
                .await
                .is_err()
        );
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_startup_cannot_observe_the_returning_route() {
        let reads = Cell::new(0);
        let ready = Cell::new(false);
        let mut old = Box::pin(wait_for_linux_default(|| {
            reads.set(reads.get() + 1);
            Ok(ready.get().then(|| "eth-test".into()))
        }));
        tokio::select! {
            result = &mut old => panic!("old launch completed unexpectedly: {result:?}"),
            () = sleep(Duration::from_millis(600)) => {}
        }
        drop(old);
        let reads_on_stop = reads.get();
        assert!(reads_on_stop > 1);
        ready.set(true);
        advance(Duration::from_secs(4)).await;
        assert_eq!(reads.get(), reads_on_stop);
        assert_eq!(
            wait_for_linux_default(|| Ok(ready.get().then(|| "eth-test".into())))
                .await
                .unwrap(),
            "eth-test",
        );
    }
}
