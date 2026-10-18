use std::time::Duration;

use anyhow::Context;

use mdns_sd::{AddrType, ServiceDaemon, ServiceInfo};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::peer_cache::LAN_PEER_TTL;

use crate::utils::{gen_mdns_endpoint_info, gen_mdns_name, DeviceType};

const INNER_NAME: &str = "MDnsServer";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Visibility {
    Visible = 0,
    Invisible = 1,
}

pub struct MDnsServer {
    service_info: ServiceInfo,
    visibility_receiver: watch::Receiver<Visibility>,
}

impl MDnsServer {
    pub fn new(
        endpoint_id: [u8; 4],
        service_port: u16,
        visibility_receiver: watch::Receiver<Visibility>,
        hostname: &str,
    ) -> Result<Self, anyhow::Error> {
        let service_info =
            Self::build_service(endpoint_id, service_port, DeviceType::Laptop, hostname)?;

        Ok(Self {
            service_info,
            visibility_receiver,
        })
    }

    pub async fn run(&mut self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");
        while !ctk.is_cancelled() && self.visibility_receiver.has_changed().is_ok() {
            // Hidden receive mode must not retain an advertisement listener or
            // the pinned mdns-sd's otherwise persistent DNS-name cache.
            if *self.visibility_receiver.borrow_and_update() == Visibility::Invisible {
                tokio::select! {
                    _ = ctk.cancelled() => break,
                    changed = self.visibility_receiver.changed() => {
                        if changed.is_err() { break; }
                    }
                }
                continue;
            }

            let daemon = ServiceDaemon::new()?;
            let result: Result<(), anyhow::Error> = async {
                daemon.register(self.service_info.clone())?;
                // Match the browser's cache retirement period. Repeated Visible
                // notifications must not extend this generation's lifetime.
                let lifetime = tokio::time::sleep(LAN_PEER_TTL / 2);
                tokio::pin!(lifetime);
                loop {
                    tokio::select! {
                        _ = ctk.cancelled() => break,
                        _ = &mut lifetime => break,
                        changed = self.visibility_receiver.changed() => {
                            if changed.is_err()
                                || *self.visibility_receiver.borrow_and_update() == Visibility::Invisible {
                                break;
                            }
                        }
                    }
                }
                Ok(())
            }.await;

            // Retire this generation on every exit, including setup errors.
            // No monitor is subscribed: its discarded events previously added
            // a bounded channel that could obstruct daemon shutdown.
            let unregister: Result<(), anyhow::Error> = async {
                let stopped = daemon.unregister(self.service_info.get_fullname())?;
                tokio::time::timeout(Duration::from_secs(3), stopped.recv_async())
                    .await.context("mDNS advertisement unregister timed out")??;
                Ok(())
            }.await;
            // Shutdown is attempted even if unregister failed. Do not create
            // another generation until its predecessor acknowledges shutdown.
            let shutdown: Result<(), anyhow::Error> = async {
                let stopped = daemon.shutdown()?;
                tokio::time::timeout(Duration::from_secs(3), stopped.recv_async())
                    .await.context("mDNS advertisement shutdown timed out")??;
                Ok(())
            }.await;
            result?;
            unregister?;
            shutdown?;
        }
        Ok(())
    }

    fn build_service(
        endpoint_id: [u8; 4],
        service_port: u16,
        device_type: DeviceType,
        hostname: &str,
    ) -> Result<ServiceInfo, anyhow::Error> {
        let name = gen_mdns_name(endpoint_id);
        debug!("Preparing mDNS service for: {hostname}");
        let endpoint_info = gen_mdns_endpoint_info(device_type as u8, hostname);

        let properties = [("n", endpoint_info)];
        let si = ServiceInfo::new(
            "_FC9F5ED42C8A._tcp.local.",
            &name,
            hostname,
            "",
            service_port,
            &properties[..],
        )?
        .enable_addr_auto(AddrType::V4);

        Ok(si)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closing_visibility_owner_stops_hidden_advertiser() {
        let (owner, visibility) = watch::channel(Visibility::Invisible);
        let mut server = MDnsServer::new(*b"TEST", 1234, visibility, "hidden.local.").unwrap();
        let run = server.run(CancellationToken::new());
        tokio::pin!(run);
        tokio::select! {
            _ = &mut run => panic!("advertiser stopped while its visibility owner was alive"),
            _ = tokio::task::yield_now() => {}
        }
        drop(owner);
        tokio::time::timeout(Duration::from_secs(2), run)
            .await.expect("closed visibility owner must terminate the actor")
            .expect("hidden advertiser shutdown");
    }
}
