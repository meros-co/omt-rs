//! OMT source discovery + advertising over mDNS/DNS-SD (`_omt._tcp`).
//!
//! OMT is a discovery-first protocol (like NDI): senders advertise a named
//! service and receivers browse for it by name, then resolve to host:port.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

const SERVICE_TYPE: &str = "_omt._tcp.local.";

/// This device's name, as it should appear in an OMT source name. Set it at
/// startup where the OS hostname is unusable - on Android `gethostname()`
/// returns "localhost" - or to match the name another protocol (NDI) uses.
static MACHINE_NAME: Mutex<Option<String>> = Mutex::new(None);

/// Prefix of the synthetic mDNS host name an [`Advertiser`] publishes
/// (`{prefix}-{port}.local.`). Cosmetic; receivers only use its addresses.
static HOST_PREFIX: Mutex<Option<String>> = Mutex::new(None);

pub fn set_advertised_host_prefix(prefix: &str) {
    let cleaned: String = prefix
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    *HOST_PREFIX.lock().unwrap() = (!cleaned.is_empty()).then_some(cleaned);
}

pub fn set_machine_name(name: &str) {
    let cleaned = name.trim().trim_matches('.').to_uppercase();
    if !cleaned.is_empty() {
        *MACHINE_NAME.lock().unwrap() = Some(cleaned);
    }
}

/// The discovery name OMT uses for a source: `MACHINE (Name)`.
///
/// Mandatory, not cosmetic: libomt's `OMTAddress.IsValid()` requires both
/// parentheses and `Create()` returns null without them, so a receiver
/// silently DISCARDS a bare-named record - it can never appear in its source
/// list. Dots are stripped from the label exactly as libomt does, because
/// Windows DNS-SD rejects them.
pub fn full_source_name(name: &str) -> String {
    let machine = MACHINE_NAME
        .lock()
        .unwrap()
        .clone()
        .or_else(|| {
            // Desktop default. Mobile apps should call set_machine_name:
            // neither variable exists there.
            std::env::var("COMPUTERNAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .ok()
        })
        .unwrap_or_else(|| "OMT".to_string());
    let machine = machine.split('.').next().unwrap_or("OMT").to_uppercase();
    format!("{machine} ({})", name.replace('.', ""))
}

/// The OMT source name of a resolved service, e.g. "HOST (Source)" — the
/// instance label with the `._omt._tcp.local.` suffix stripped.
fn instance_name(info: &ServiceInfo) -> String {
    let full = info.get_fullname();
    full.strip_suffix(&format!(".{SERVICE_TYPE}"))
        .unwrap_or(full)
        .replace("\\ ", " ")
        .replace("\\.", ".")
}

/// Browses for OMT sources on the LAN, returning their names (sorted, unique).
/// Blocks up to `timeout_ms`.
pub fn discover(timeout_ms: u64) -> Vec<String> {
    let Ok(daemon) = ServiceDaemon::new() else {
        return Vec::new();
    };
    crate::net::restrict_mdns(&daemon);
    let Ok(rx) = daemon.browse(SERVICE_TYPE) else {
        return Vec::new();
    };
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut names = BTreeSet::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                names.insert(instance_name(&info));
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    names.into_iter().collect()
}

/// Resolves an OMT source name to a `host:port` string. Blocks up to
/// `timeout_ms`. Returns the first usable address.
pub fn resolve(name: &str, timeout_ms: u64) -> Option<String> {
    let daemon = ServiceDaemon::new().ok()?;
    crate::net::restrict_mdns(&daemon);
    let rx = daemon.browse(SERVICE_TYPE).ok()?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    // Prefer a routable IPv4 address. mdns-sd returns an unordered address set
    // that often includes link-local IPv6 (fe80::…) — which can't be dialed
    // without a scope id and needs bracket notation — so never just take the
    // first. Keep a bracketed global-IPv6 fallback but wait for IPv4.
    let mut result: Option<String> = None;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if instance_name(&info) == name {
                    let port = info.get_port();
                    let addrs = info.get_addresses();
                    if let Some(v4) = addrs.iter().find(|a| a.is_ipv4()) {
                        result = Some(format!("{v4}:{port}"));
                        break;
                    }
                    if result.is_none() {
                        if let Some(v6) = addrs.iter().find(|a| is_global_v6(a)) {
                            result = Some(format!("[{v6}]:{port}"));
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    result
}

/// A non-link-local IPv6 address (dialable without a scope id).
fn is_global_v6(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
        IpAddr::V4(_) => false,
    }
}

/// A registered `_omt._tcp` advertisement, so a sender is discoverable by
/// every OMT receiver while it runs; unregisters on drop.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    /// Advertises `name` on `port`. Addresses are auto-detected from the host's
    /// interfaces (`enable_addr_auto`), so no IP needs to be supplied.
    ///
    /// The record is published as `MACHINE (name)` - see [`full_source_name`].
    /// Publishing the bare `name` makes the source invisible to every libomt
    /// receiver, which is to say to every other OMT application.
    pub fn new(name: &str, port: u16) -> Option<Self> {
        let daemon = ServiceDaemon::new().ok()?;
        crate::net::restrict_mdns(&daemon);
        // A synthetic, unique host name; the actual A records are filled in by
        // enable_addr_auto() from the live interfaces.
        let prefix = HOST_PREFIX
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "omt".into());
        let host = format!("{prefix}-{port}.local.");
        // With an interface chosen, advertise that address alone; otherwise
        // let mdns-sd fill in every live interface.
        let info = match crate::net::preferred_interface() {
            Some(ip) => ServiceInfo::new(
                SERVICE_TYPE,
                &full_source_name(name),
                &host,
                ip.to_string().as_str(),
                port,
                None,
            )
            .ok()?,
            None => ServiceInfo::new(SERVICE_TYPE, &full_source_name(name), &host, "", port, None)
                .ok()?
                .enable_addr_auto(),
        };
        let fullname = info.get_fullname().to_string();
        daemon.register(info).ok()?;
        Some(Self { daemon, fullname })
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod diag {
    //! Opt-in diagnostic for a discovery that finds nothing: prints every
    //! event mdns-sd emits, not just ServiceResolved, so a browse that sees
    //! the record but fails to resolve it is distinguishable from one that
    //! never sees it at all.
    //!
    //!     OMT_MDNS_DIAG=1 cargo test mdns_event_dump -- --nocapture

    use super::*;

    #[test]
    fn mdns_event_dump() {
        if std::env::var("OMT_MDNS_DIAG").is_err() {
            eprintln!("OMT_MDNS_DIAG not set - skipping");
            return;
        }
        let daemon = ServiceDaemon::new().expect("ServiceDaemon::new");
        let rx = daemon.browse(SERVICE_TYPE).expect("browse");
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut count = 0;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(remaining) {
                Ok(ev) => {
                    count += 1;
                    match ev {
                        ServiceEvent::ServiceFound(ty, name) => {
                            eprintln!("  ServiceFound   {ty} {name}")
                        }
                        ServiceEvent::ServiceResolved(info) => eprintln!(
                            "  ServiceResolved {} port={} addrs={:?}",
                            info.get_fullname(),
                            info.get_port(),
                            info.get_addresses()
                        ),
                        ServiceEvent::SearchStarted(s) => eprintln!("  SearchStarted  {s}"),
                        ServiceEvent::SearchStopped(s) => eprintln!("  SearchStopped  {s}"),
                        ServiceEvent::ServiceRemoved(ty, name) => {
                            eprintln!("  ServiceRemoved {ty} {name}")
                        }
                    }
                }
                Err(e) => {
                    eprintln!("  recv ended: {e}");
                    break;
                }
            }
        }
        eprintln!("total events: {count}");
        let _ = daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_names_carry_the_machine_and_lose_dots() {
        set_machine_name("studio-pc.local");
        assert_eq!(full_source_name("Program 1.0"), "STUDIO-PC (Program 10)");
    }

    #[test]
    fn link_local_v6_is_not_dialable() {
        assert!(!is_global_v6(&"fe80::1".parse().unwrap()));
        assert!(is_global_v6(&"2001:db8::1".parse().unwrap()));
        assert!(!is_global_v6(&"192.168.1.2".parse().unwrap()));
    }
}
