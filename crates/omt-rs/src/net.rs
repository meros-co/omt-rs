//! Process-wide network setting: which interface OMT binds, advertises and
//! browses on. (The machine name sources are published under is set in
//! [`crate::discovery`].)
//!
//! A single-homed device needs neither. A PC usually does: with Hyper-V, WSL
//! or Docker it has several addresses that all look routable, and the default
//! (bind `0.0.0.0`, advertise every interface) lets a receiver be handed an
//! address it cannot reach. That failure is silent and looks like a broken
//! source. Choosing an interface pins the sender's listener, its mDNS
//! advertisement and discovery's browsing to that one address.
//!
//! Outbound connections (a receiver dialling a sender) are left to the OS
//! routing table, which already picks the right source address.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;

static PREFERRED: Mutex<Option<Ipv4Addr>> = Mutex::new(None);

/// Restricts OMT to one interface, by IPv4 address. An empty or unparseable
/// string clears the restriction (bind and advertise everywhere), which is the
/// default.
pub fn set_preferred_interface(ip: &str) {
    *PREFERRED.lock().unwrap() = ip.trim().parse().ok();
}

pub fn preferred_interface() -> Option<Ipv4Addr> {
    *PREFERRED.lock().unwrap()
}

/// The address a listening socket binds: the chosen interface, or `0.0.0.0`.
pub fn bind_ip() -> Ipv4Addr {
    preferred_interface().unwrap_or(Ipv4Addr::UNSPECIFIED)
}

/// [`bind_ip`] with a port.
pub fn bind_addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(bind_ip()), port)
}

/// Pins an mDNS daemon to the chosen interface, so browsing and advertising
/// happen on one network rather than every virtual adapter.
///
/// No-op when nothing is chosen. Errors are swallowed deliberately: failing to
/// narrow the interface set is better handled by discovering on all of them
/// than by giving up discovery entirely.
pub fn restrict_mdns(daemon: &mdns_sd::ServiceDaemon) {
    let Some(ip) = preferred_interface() else {
        return;
    };
    use mdns_sd::IfKind;
    if let Err(e) = daemon.disable_interface(IfKind::All) {
        log::warn!("could not disable mDNS interfaces: {e}");
        return;
    }
    if let Err(e) = daemon.enable_interface(IfKind::from(IpAddr::V4(ip))) {
        log::warn!("could not pin mDNS to {ip}: {e}");
        let _ = daemon.enable_interface(IfKind::All);
    }
}

/// Serialises tests that touch the process-global interface setting.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn unset_means_bind_everything() {
        let _g = lock();
        set_preferred_interface("");
        assert_eq!(bind_ip(), Ipv4Addr::UNSPECIFIED);
        assert_eq!(bind_addr(1234).port(), 1234);
    }

    #[test]
    fn a_chosen_interface_is_what_sockets_bind() {
        let _g = lock();
        set_preferred_interface("192.168.1.216");
        assert_eq!(bind_ip(), Ipv4Addr::new(192, 168, 1, 216));
        assert_eq!(
            bind_addr(6960),
            "192.168.1.216:6960".parse::<SocketAddr>().unwrap()
        );
        set_preferred_interface("");
    }

    #[test]
    fn garbage_clears_the_restriction() {
        let _g = lock();
        set_preferred_interface("192.168.1.216");
        set_preferred_interface("not-an-ip");
        assert_eq!(preferred_interface(), None);
    }

    #[test]
    fn ipv6_is_rejected() {
        let _g = lock();
        set_preferred_interface("fe80::1");
        assert_eq!(preferred_interface(), None);
        set_preferred_interface("");
    }
}
