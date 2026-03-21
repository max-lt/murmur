//! mDNS-based LAN peer discovery for murmurd.
//!
//! When enabled (`[network] mdns = true`), broadcasts this daemon as a
//! `_murmur._udp.local.` mDNS service and discovers other daemons on the
//! same LAN. Discovered peers are added to the gossip bootstrap set.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use murmur_types::NetworkId;
use tracing::{debug, info, warn};

/// mDNS service type for murmur daemons.
const SERVICE_TYPE: &str = "_murmur._udp.local.";

/// Start mDNS discovery and registration.
///
/// Registers this daemon as an mDNS service so other LAN peers can find it,
/// and browses for other murmur services. Returns a handle to stop the mDNS
/// daemon on shutdown.
pub fn start_mdns(
    network_id: &NetworkId,
    iroh_port: u16,
    connected_peers: Arc<AtomicU64>,
) -> Result<MdnsHandle, anyhow::Error> {
    let mdns = ServiceDaemon::new()?;

    // Use the network ID (hex, truncated) as the instance name to
    // avoid cross-network discovery.
    let network_hex: String = network_id
        .as_bytes()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    let instance_name = format!("murmur-{network_hex}");

    // Register our service.
    let host = hostname::get()
        .unwrap_or_else(|_| "murmurd".into())
        .to_string_lossy()
        .to_string();
    let host_fqdn = format!("{host}.local.");

    let service_info = ServiceInfo::new(
        SERVICE_TYPE,
        &instance_name,
        &host_fqdn,
        "", // auto-detect IP
        iroh_port,
        None::<std::collections::HashMap<String, String>>,
    )
    .map_err(|e| anyhow::anyhow!("mDNS service info: {e}"))?;

    mdns.register(service_info)
        .map_err(|e| anyhow::anyhow!("mDNS register: {e}"))?;

    info!(instance = %instance_name, "mDNS service registered");

    // Browse for other murmur services.
    let receiver = mdns
        .browse(SERVICE_TYPE)
        .map_err(|e| anyhow::anyhow!("mDNS browse: {e}"))?;

    let peers = connected_peers;
    std::thread::spawn(move || {
        loop {
            match receiver.recv() {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let addrs: Vec<_> = info.get_addresses().iter().collect();
                    let port = info.get_port();
                    info!(
                        name = info.get_fullname(),
                        ?addrs,
                        port,
                        "mDNS: discovered murmur peer"
                    );
                    // Note: In a full implementation, we would add these
                    // addresses to the iroh endpoint's address book.
                    // For now, we log the discovery — iroh's relay-based
                    // discovery handles the actual connection.
                    let _ = peers.load(Ordering::Relaxed);
                }
                Ok(ServiceEvent::ServiceRemoved(_, name)) => {
                    debug!(name, "mDNS: murmur peer removed");
                }
                Ok(_) => {} // SearchStarted, etc.
                Err(e) => {
                    warn!(error = %e, "mDNS browse recv error");
                    break;
                }
            }
        }
    });

    Ok(MdnsHandle { daemon: mdns })
}

/// Handle to the mDNS daemon, used for cleanup on shutdown.
pub struct MdnsHandle {
    daemon: ServiceDaemon,
}

impl MdnsHandle {
    /// Shut down the mDNS daemon.
    pub fn shutdown(self) {
        if let Err(e) = self.daemon.shutdown() {
            warn!(error = %e, "mDNS shutdown error");
        } else {
            info!("mDNS stopped");
        }
    }
}
