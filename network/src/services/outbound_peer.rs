use crate::NetworkState;
use futures::{try_ready, Async, Stream};
use log::{debug, trace, warn};
use p2p::service::ServiceControl;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::usize;
use tokio::timer::Interval;

const FEELER_CONNECTION_COUNT: u32 = 5;

pub struct OutboundPeerService {
    pub stream_interval: Interval,
    pub network_state: Arc<NetworkState>,
    pub p2p_control: ServiceControl,
}

impl OutboundPeerService {
    pub fn new(
        network_state: Arc<NetworkState>,
        p2p_control: ServiceControl,
        try_connect_interval: Duration,
    ) -> Self {
        OutboundPeerService {
            network_state,
            p2p_control,
            stream_interval: Interval::new_interval(try_connect_interval),
        }
    }

    fn attempt_dial_peers(&mut self, count: u32) {
        let attempt_peers = self
            .network_state
            .with_peer_store(|peer_store| peer_store.peers_to_attempt(count + 5));
        let p2p_control = self.p2p_control.clone();
        trace!(target: "network", "count={}, attempt_peers: {:?}", count, attempt_peers);
        for (peer_id, addr) in attempt_peers
            .into_iter()
            .filter(|(peer_id, _addr)| {
                self.network_state.local_peer_id() != peer_id
                    && !self
                        .network_state
                        .with_peer_registry(|reg| reg.is_feeler(peer_id))
                    && self
                        .network_state
                        .failed_dials
                        .read()
                        .get(peer_id)
                        .map(|last_dial| {
                            // Dial after 5 minutes when last failed
                            Instant::now() - *last_dial > Duration::from_secs(300)
                        })
                        .unwrap_or(true)
            })
            .take(count as usize)
        {
            debug!(target: "network", "dial attempt peer: {:?}", addr);
            self.network_state.dial_all(&p2p_control, &peer_id, addr);
        }
    }

    fn feeler_peers(&mut self, count: u32) {
        let peers = self
            .network_state
            .with_peer_store(|peer_store| peer_store.peers_to_feeler(count));
        let p2p_control = self.p2p_control.clone();
        for (peer_id, addr) in peers
            .into_iter()
            .filter(|(peer_id, _addr)| self.network_state.local_peer_id() != peer_id)
        {
            self.network_state.with_peer_registry_mut(|reg| {
                reg.add_feeler(peer_id.clone());
            });
            debug!(target: "network", "dial feeler peer: {:?}", addr);
            self.network_state.dial_feeler(&p2p_control, &peer_id, addr);
        }
    }
}

impl Stream for OutboundPeerService {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Result<Async<Option<Self::Item>>, Self::Error> {
        match try_ready!(self.stream_interval.poll().map_err(|_| ())) {
            Some(_tick) => {
                let status = self.network_state.connection_status();
                let new_outbound = status.max_outbound - status.unreserved_outbound;
                if new_outbound > 0 {
                    // dial peers
                    self.attempt_dial_peers(new_outbound as u32);
                } else {
                    // feeler peers
                    self.feeler_peers(FEELER_CONNECTION_COUNT);
                }
            }
            None => {
                warn!(target: "network", "ckb outbound peer service stopped");
                return Ok(Async::Ready(None));
            }
        }
        Ok(Async::Ready(Some(())))
    }
}
