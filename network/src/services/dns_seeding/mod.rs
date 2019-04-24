use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use faster_hex::hex_decode;
use futures::{Async, Future, Poll, Stream};
use log::{debug, error, info, trace, warn};
use p2p::{
    multiaddr::{Protocol, ToMultiaddr},
    secio::PeerId,
};
use resolve::record::Txt;
use resolve::{DnsConfig, DnsResolver};
use secp256k1::key::PublicKey;
use tokio::timer::Interval;

mod seed_record;

use crate::NetworkState;
use seed_record::SeedRecord;

// FIXME: should replace this later
const TXT_VERIFY_PUBKEY: &str = "33afa0d4309e4720ba60b29e63c4f378fef860bcfe14732fd2790107c4237ca92244ec8c76e013ba7d88499288ef94ff412b5c8bf239fbb70488d5f6fbbc75a2";

pub(crate) struct DnsSeedingService {
    network_state: Arc<NetworkState>,
    wait_until: Instant,
    // Because tokio timer is not reliable
    check_interval: Interval,
    seeds: Vec<String>,
}

impl DnsSeedingService {
    pub(crate) fn new(network_state: Arc<NetworkState>, seeds: Vec<String>) -> DnsSeedingService {
        let wait_until =
            if network_state.with_peer_store(|peer_store| peer_store.random_peers(1).is_empty()) {
                info!(target: "network", "No peer in peer store, start seeding...");
                Instant::now()
            } else {
                Instant::now() + Duration::from_secs(11)
            };
        let check_interval = Interval::new_interval(Duration::from_secs(1));
        DnsSeedingService {
            network_state,
            wait_until,
            check_interval,
            seeds,
        }
    }

    fn seeding(&self) -> Result<(), Box<dyn Error>> {
        let enough_outbound = self.network_state.with_peer_registry(|reg| {
            reg.peers()
                .values()
                .filter(|peer| peer.is_outbound())
                .count()
                >= 2
        });
        if enough_outbound {
            debug!(target: "network", "Enough outbound peers");
            return Ok(());
        }

        let mut pubkey_bytes = [4u8; 65];
        hex_decode(TXT_VERIFY_PUBKEY.as_bytes(), &mut pubkey_bytes[1..65])
            .map_err(|err| format!("parse key({}) error: {:?}", TXT_VERIFY_PUBKEY, err))?;
        let pubkey = PublicKey::from_slice(&pubkey_bytes)
            .map_err(|err| format!("create PublicKey failed: {:?}", err))?;

        let resolver = DnsConfig::load_default()
            .map_err(|err| format!("Failed to load system configuration: {}", err))
            .and_then(|config| {
                DnsResolver::new(config)
                    .map_err(|err| format!("Failed to create DNS resolver: {}", err))
            })?;

        let mut addrs = Vec::new();
        for seed in &self.seeds {
            debug!(target: "network", "query txt records from: {}", seed);
            match resolver.resolve_record::<Txt>(seed) {
                Ok(records) => {
                    for record in records {
                        match std::str::from_utf8(&record.data) {
                            Ok(record) => match SeedRecord::decode_with_pubkey(&record, &pubkey) {
                                Ok(seed_record) => {
                                    let address = seed_record.address();
                                    trace!(target: "network", "got dns txt address: {}", address);
                                    addrs.push(address);
                                }
                                Err(err) => {
                                    debug!(target: "network", "decode dns txt record failed: {:?}, {:?}", err, record);
                                }
                            },
                            Err(err) => {
                                debug!(target: "network", "get dns txt record error: {:?}", err);
                            }
                        }
                    }
                }
                Err(_) => {
                    if let Ok(addr) = seed.to_multiaddr() {
                        debug!(target: "network", "DNS query failed, {} is a multiaddr", addr);
                        addrs.push(addr);
                    } else {
                        warn!(target: "network", "Invalid domain name or multiaddr: {}", seed);
                    }
                }
            }
        }

        debug!(target: "network", "DNS seeding got {} address", addrs.len());
        self.network_state.with_peer_store_mut(|peer_store| {
            for mut addr in addrs {
                match addr.pop() {
                    Some(Protocol::P2p(key)) => {
                        if let Ok(peer_id) = PeerId::from_bytes(key.into_bytes()) {
                            peer_store.add_discovered_addr(&peer_id, addr);
                        }
                    }
                    _ => {
                        debug!(target: "network", "Got addr without peer_id: {}", addr);
                    }
                }
            }
        });
        Ok(())
    }
}

impl Future for DnsSeedingService {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self.check_interval.poll() {
                Ok(Async::Ready(Some(_))) => {
                    if self.wait_until < Instant::now() {
                        if let Err(err) = self.seeding() {
                            error!(target: "network", "seeding error: {:?}", err);
                        }
                        debug!(target: "network", "DNS seeding finished");
                        return Ok(Async::Ready(()));
                    } else {
                        trace!(target: "network", "DNS check interval");
                    }
                }
                Ok(Async::Ready(None)) => {
                    warn!(target: "network", "Poll DnsSeedingService interval return None");
                    return Err(());
                }
                Ok(Async::NotReady) => break,
                Err(err) => {
                    warn!(target: "network", "Poll DnsSeedingService interval error: {:?}", err);
                    return Err(());
                }
            }
        }
        Ok(Async::NotReady)
    }
}
