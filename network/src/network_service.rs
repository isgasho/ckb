use crate::errors::{Error, ProtocolError};
use crate::peer_store::{sqlite::SqlitePeerStore, PeerStore, Status};
use crate::peers_registry::{ConnectionStatus, PeersRegistry};
use crate::protocols::{
    discovery::{DiscoveryProtocol, DiscoveryService},
    identify::IdentifyCallback,
    outbound_peer::OutboundPeerService,
    ping::PingService,
};
use crate::protocols::{feeler::Feeler, BackgroundService, DefaultCKBProtocolContext};
use crate::MultiaddrList;
use crate::Peer;
use crate::{
    Behaviour, CKBProtocol, CKBProtocolContext, NetworkConfig, NetworkState, PeerIndex, ProtocolId,
    ProtocolVersion, ServiceContext, ServiceControl, SessionId, SessionType,
};
use crate::{DISCOVERY_PROTOCOL_ID, FEELER_PROTOCOL_ID, IDENTIFY_PROTOCOL_ID, PING_PROTOCOL_ID};
use ckb_core::service::{Request, DEFAULT_CHANNEL_SIZE, SIGNAL_CHANNEL_SIZE};
use ckb_util::RwLock;
use crossbeam_channel::{self, select, Receiver, Sender, TryRecvError};
use fnv::{FnvHashMap, FnvHashSet};
use futures::sync::mpsc::channel;
use futures::sync::{mpsc, oneshot};
use futures::Future;
use futures::Stream;
use futures::{try_ready, Async, Poll};
use log::{debug, error, info, trace, warn};
use lru_cache::LruCache;
use p2p::{
    builder::{MetaBuilder, ServiceBuilder},
    error::Error as P2pError,
    multiaddr::{self, multihash::Multihash, Multiaddr},
    secio::PeerId,
    service::{DialProtocol, ProtocolEvent, ProtocolHandle, Service, ServiceError, ServiceEvent},
    traits::ServiceHandle,
    utils::extract_peer_id,
};
use p2p_identify::IdentifyProtocol;
use p2p_ping::PingHandler;
use secio;
use std::boxed::Box;
use std::cell::RefCell;
use std::cmp::max;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use std::usize;
use stop_handler::{SignalSender, StopHandler};
use tokio::runtime::Runtime;

pub struct EventHandler {
    sender: mpsc::UnboundedSender<NetworkEvent>,
}

impl ServiceHandle for EventHandler {
    fn handle_error(&mut self, _context: &mut ServiceContext, error: ServiceError) {
        warn!(target: "network", "p2p service error: {:?}", error);
        match self.sender.unbounded_send(NetworkEvent::Error(error)) {
            Ok(_) => {
                trace!(target: "network", "send network error success");
            }
            Err(err) => error!(target: "network", "send network error failed: {:?}", err),
        }
    }

    fn handle_event(&mut self, context: &mut ServiceContext, event: ServiceEvent) {
        info!(target: "network", "p2p service event: {:?}", event);
        match self.sender.unbounded_send(NetworkEvent::Event(event)) {
            Ok(_) => {
                trace!(target: "network", "send network service event success");
            }
            Err(err) => error!(target: "network", "send network event failed: {:?}", err),
        }
    }

    fn handle_proto(&mut self, context: &mut ServiceContext, event: ProtocolEvent) {
        match self.sender.unbounded_send(NetworkEvent::Protocol(event)) {
            Ok(_) => {
                trace!(target: "network", "send network protocol event success");
            }
            Err(err) => error!(target: "network", "send network event failed: {:?}", err),
        }
    }
}

enum NetworkEvent {
    Protocol(ProtocolEvent),
    Event(ServiceEvent),
    Error(ServiceError),
}

pub struct NetworkService {
    event_receiver: mpsc::UnboundedReceiver<NetworkEvent>,
    p2p_control: ServiceControl,
    network_state: RefCell<NetworkState>,
    // Background services
    bg_services: Vec<Box<dyn BackgroundService + Send + 'static>>,
    protocols: Vec<CKBProtocol>,
    receivers: NetworkReceivers,
    stop_signal: Receiver<Sender<()>>,
}

impl Stream for NetworkService {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // handle all network events
        loop {
            match self.event_receiver.poll() {
                Ok(Async::Ready(Some(NetworkEvent::Error(error)))) => {
                    self.handle_service_error(error);
                }
                Ok(Async::Ready(Some(NetworkEvent::Event(event)))) => {
                    self.handle_service_event(event);
                }

                Ok(Async::Ready(Some(NetworkEvent::Protocol(event)))) => {
                    self.handle_protocol(event);
                }
                Ok(Async::Ready(None)) => {
                    error!(target: "network", "event_receiver stopped");
                }
                Ok(Async::NotReady) => break,
                Err(err) => {
                    error!(target: "network", "event_receiver error : {:?}", err);
                }
            }
        }

        // handle back ground services
        {
            let mut network_state = self.network_state.borrow_mut();
            for s in &mut self.bg_services {
                s.poll(&mut network_state);
            }
            // clean peers by is_disconnect flag
            network_state.drop_disconnect_peers(&mut self.p2p_control);
        }

        // handle controller request
        self.process_rpc_call();

        // Check Shutdown
        match self.stop_signal.try_recv() {
            Err(TryRecvError::Empty) => Ok(Async::Ready(Some(()))),
            Ok(stop_waiter) => {
                debug!(target: "network", "network received stop signal");
                self.shutdown();
                stop_waiter.send(());
                Ok(Async::Ready(None))
            }
            Err(TryRecvError::Disconnected) => {
                debug!(target: "network", "network stop signal dropped");
                self.shutdown();
                Ok(Async::Ready(None))
            }
        }
    }
}

impl NetworkService {
    pub fn build(
        mut network_state: NetworkState,
        protocols: Vec<CKBProtocol>,
    ) -> (NetworkService, Service<EventHandler>, NetworkController) {
        let config = network_state.config.clone();

        // == Build NetworkController
        let (external_urls_sender, external_urls_receiver) =
            crossbeam_channel::bounded(DEFAULT_CHANNEL_SIZE);
        let (dial_node_sender, dial_node_receiver) =
            crossbeam_channel::bounded(DEFAULT_CHANNEL_SIZE);
        let (connected_peers_sender, connected_peers_receiver) =
            crossbeam_channel::bounded(DEFAULT_CHANNEL_SIZE);
        let (listened_addresses_sender, listened_addresses_receiver) =
            crossbeam_channel::bounded(DEFAULT_CHANNEL_SIZE);
        let (add_discovered_addr_sender, add_discovered_addr_receiver) =
            crossbeam_channel::bounded(DEFAULT_CHANNEL_SIZE);
        let (stop_sender, stop_signal) = crossbeam_channel::bounded(1);

        let receivers = NetworkReceivers {
            external_urls_receiver,
            listened_addresses_receiver,
            dial_node_receiver,
            connected_peers_receiver,
            add_discovered_addr_receiver,
        };
        let controller = NetworkController {
            peer_id: network_state.local_peer_id().to_owned(),
            external_urls_sender,
            listened_addresses_sender,
            dial_node_sender,
            connected_peers_sender,
            add_discovered_addr_sender,
            stop_sender,
        };

        // == Build special protocols

        // TODO: how to deny banned node to open those protocols?
        // Ping protocol
        let (ping_sender, ping_receiver) = channel(std::u8::MAX as usize);
        let ping_meta = MetaBuilder::default()
            .id(PING_PROTOCOL_ID)
            .service_handle({
                let ping_sender = ping_sender.clone();
                let ping_interval_secs = config.ping_interval_secs;
                let ping_timeout_secs = config.ping_timeout_secs;
                move || {
                    ProtocolHandle::Both(Box::new(PingHandler::new(
                        PING_PROTOCOL_ID,
                        Duration::from_secs(ping_interval_secs),
                        Duration::from_secs(ping_timeout_secs),
                        ping_sender.clone(),
                    )))
                }
            })
            .build();

        // Discovery protocol
        let (disc_sender, disc_receiver) = mpsc::unbounded();
        let disc_meta = MetaBuilder::default()
            .id(DISCOVERY_PROTOCOL_ID)
            .service_handle({
                let disc_sender = disc_sender.clone();
                move || ProtocolHandle::Both(Box::new(DiscoveryProtocol::new(disc_sender.clone())))
            })
            .build();

        // Identify protocol
        let identify_meta = MetaBuilder::default()
            .id(IDENTIFY_PROTOCOL_ID)
            .service_handle({
                let controller = controller.clone();
                move || {
                    let identify_callback = IdentifyCallback::new(controller.clone());
                    ProtocolHandle::Both(Box::new(IdentifyProtocol::new(identify_callback)))
                }
            })
            .build();

        // Feeler protocol
        let feeler_protocol = CKBProtocol::new(
            "flr".to_string(),
            FEELER_PROTOCOL_ID,
            &[1][..],
            Box::new(Feeler {}),
        );

        // == Build p2p service struct
        let mut protocol_metas = protocols
            .iter()
            .map(|protocol| protocol.build())
            .collect::<Vec<_>>();
        protocol_metas.push(feeler_protocol.build());
        protocol_metas.push(ping_meta);
        protocol_metas.push(disc_meta);
        protocol_metas.push(identify_meta);

        let mut service_builder = ServiceBuilder::default();
        for meta in protocol_metas.into_iter() {
            network_state.protocol_ids.insert(meta.id());
            service_builder = service_builder.insert_protocol(meta);
        }

        let (event_sender, event_receiver) = mpsc::unbounded();

        let event_handler = EventHandler {
            sender: event_sender,
        };
        let mut p2p_service = service_builder
            .key_pair(network_state.local_private_key().clone())
            .forever(true)
            .build(event_handler);

        // == Build background service tasks
        let disc_service = DiscoveryService::new(disc_receiver);
        let ping_service = PingService::new(p2p_service.control().clone(), ping_receiver);
        let outbound_peer_service = OutboundPeerService::new(
            p2p_service.control().clone(),
            Duration::from_secs(config.connect_outbound_interval_secs),
        );
        let bg_services = vec![
            Box::new(ping_service) as Box<_>,
            Box::new(disc_service) as Box<_>,
            Box::new(outbound_peer_service) as Box<_>,
        ];

        let network_service = NetworkService {
            p2p_control: p2p_service.control().clone(),
            network_state: RefCell::new(network_state),
            bg_services,
            protocols,
            event_receiver,
            receivers,
            stop_signal,
        };
        (network_service, p2p_service, controller)
    }

    pub fn start(
        mut network_service: NetworkService,
        mut p2p_service: Service<EventHandler>,
    ) -> Result<(), Error> {
        network_service.setup_network(&mut p2p_service)?;
        // spawn p2p and network service
        let mut runtime = Runtime::new().expect("Network tokio runtime init failed");
        runtime.spawn(p2p_service.for_each(|_| Ok(())));
        runtime.spawn(network_service.for_each(|_| Ok(())));
        Ok(())
    }

    fn setup_network(&mut self, p2p_service: &mut Service<EventHandler>) -> Result<(), Error> {
        let config = &self.network_state.borrow().config;
        // listen local addresses
        for addr in &config.listen_addresses {
            match p2p_service.listen(addr.to_owned()) {
                Ok(listen_address) => {
                    let mut network_state = self.network_state.borrow_mut();
                    info!(
                    target: "network",
                    "Listen on address: {}",
                    network_state.to_external_url(&listen_address)
                    );
                    network_state
                        .original_listened_addresses
                        .push(listen_address.clone())
                }
                Err(err) => {
                    warn!(
                    target: "network",
                    "listen on address {} failed, due to error: {}",
                    addr.clone(),
                    err
                    );
                    return Err(Error::Io(err));
                }
            };
        }

        // dial reserved_nodes
        for (peer_id, addr) in config.reserved_peers()? {
            debug!(target: "network", "dial reserved_peers {:?} {:?}", peer_id, addr);
            self.network_state
                .borrow_mut()
                .dial_all(p2p_service.control(), &peer_id, addr);
        }

        let bootnodes = self
            .network_state
            .borrow()
            .peer_store()
            .bootnodes(max((config.max_outbound_peers / 2) as u32, 1))
            .clone();
        // dial half bootnodes
        for (peer_id, addr) in bootnodes {
            debug!(target: "network", "dial bootnode {:?} {:?}", peer_id, addr);
            self.network_state
                .borrow_mut()
                .dial_all(p2p_service.control(), &peer_id, addr);
        }
        Ok(())
    }

    fn handle_service_event(&mut self, event: ServiceEvent) {
        let mut network_state = self.network_state.borrow_mut();
        match event {
            // Register Peer
            ServiceEvent::SessionOpen { session_context } => {
                let peer_id = session_context
                    .remote_pubkey
                    .as_ref()
                    .map(|pubkey| pubkey.peer_id())
                    .expect("Secio must enabled");
                // try accept connection
                if let Err(err) = network_state.accept_connection(
                    peer_id.clone(),
                    session_context.address.clone(),
                    session_context.id,
                    session_context.ty,
                ) {
                    // disconnect immediatly
                    self.p2p_control.disconnect(session_context.id);
                    info!(
                    target: "network",
                    "reject connection from {} {}, because {:?}",
                    peer_id.to_base58(),
                    session_context.address,
                    err,
                    );
                }
            }
            // When session disconnect update status anyway
            ServiceEvent::SessionClose { session_context } => {
                let peer_id = session_context
                    .remote_pubkey
                    .as_ref()
                    .map(|pubkey| pubkey.peer_id())
                    .expect("Secio must enabled");
                network_state.disconnect_peer(&peer_id);
            }
            _ => {
                // do nothing
            }
        }
    }

    fn handle_service_error(&self, error: ServiceError) {
        if let ServiceError::DialerError {
            ref address,
            ref error,
        } = error
        {
            debug!(target: "network", "add self address: {:?}", address);
            if error == &P2pError::ConnectSelf {
                let addr = address
                    .iter()
                    .filter(|proto| match proto {
                        multiaddr::Protocol::P2p(_) => false,
                        _ => true,
                    })
                    .collect();
                self.network_state
                    .borrow_mut()
                    .listened_addresses
                    .insert(addr, std::u8::MAX);
            }
            // TODO implement in peer store
            if let Some(peer_id) = extract_peer_id(address) {
                self.network_state
                    .borrow_mut()
                    .failed_dials
                    .insert(peer_id, Instant::now());
            }
        }
    }

    fn handle_protocol(&mut self, event: ProtocolEvent) {
        let p2p_control = self.p2p_control.clone();
        let mut network_state = self.network_state.borrow_mut();
        match event {
            ProtocolEvent::Connected {
                session_context,
                proto_id,
                version,
            } => {
                let peer_id = session_context
                    .remote_pubkey
                    .as_ref()
                    .map(|pubkey| pubkey.peer_id())
                    .expect("Secio must enabled");
                let proto_version = version
                    .parse::<ProtocolVersion>()
                    .expect("parse protocol version");
                // register new protocol
                if let Err(err) =
                    network_state.peer_new_protocol(peer_id.clone(), proto_id, proto_version)
                {
                    error!(target: "network", "disconnect peer {:?}, because {:?}",peer_id, err);
                    network_state.disconnect_peer(&peer_id);
                    return;
                } // call handler
                let protocol = self.find_protocol(proto_id).expect("protocol");
                let peer_index = network_state.get_peer_index(&peer_id).expect("peer index");
                protocol.handler().connected(
                    &mut DefaultCKBProtocolContext::new(proto_id, &mut network_state, p2p_control),
                    peer_index,
                );
            }

            ProtocolEvent::Received {
                session_context,
                proto_id,
                data,
            } => {
                let peer_id = session_context
                    .remote_pubkey
                    .as_ref()
                    .map(|pubkey| pubkey.peer_id())
                    .expect("Secio must enabled");
                if let Some(protocol) = self.find_protocol(proto_id) {
                    let peer_index = self
                        .network_state
                        .borrow()
                        .get_peer_index(&peer_id)
                        .expect("peer index");
                    protocol.handler().received(
                        &mut DefaultCKBProtocolContext::new(
                            proto_id,
                            &mut self.network_state.borrow_mut(),
                            p2p_control,
                        ),
                        peer_index,
                        data,
                    );
                }
            }
            ProtocolEvent::Disconnected {
                proto_id,
                session_context,
            } => {
                let peer_id = session_context
                    .remote_pubkey
                    .as_ref()
                    .map(|pubkey| pubkey.peer_id())
                    .expect("Secio must enabled");
                if let Some(protocol) = self.find_protocol(proto_id) {
                    let peer_index = self
                        .network_state
                        .borrow()
                        .get_peer_index(&peer_id)
                        .expect("peer index");
                    protocol.handler().disconnected(
                        &mut DefaultCKBProtocolContext::new(
                            proto_id,
                            &mut self.network_state.borrow_mut(),
                            p2p_control,
                        ),
                        peer_index,
                    );
                }
            }
            ProtocolEvent::ProtocolNotify { proto_id, token } => {
                if let Some(protocol) = self.find_protocol(proto_id) {
                    protocol.handler().timer_triggered(
                        &mut DefaultCKBProtocolContext::new(
                            proto_id,
                            &mut self.network_state.borrow_mut(),
                            p2p_control,
                        ),
                        token,
                    );
                }
            }
            ProtocolEvent::ProtocolSessionNotify {
                session_context,
                proto_id,
                token,
            } => {
                // ignore
            }
        }
    }
    fn init_protocols(&mut self) {
        let p2p_control = self.p2p_control.clone();
        for p in &self.protocols {
            p.handler().initialize(&mut DefaultCKBProtocolContext::new(
                p.id(),
                &mut self.network_state.borrow_mut(),
                p2p_control.clone(),
            ));
        }
    }

    fn find_protocol(&self, proto_id: ProtocolId) -> Option<&CKBProtocol> {
        self.protocols.iter().find(|p| p.id() == proto_id)
    }

    fn process_rpc_call(&mut self) -> bool {
        let mut network_state = self.network_state.borrow_mut();
        select! {
            recv(self.receivers.external_urls_receiver) -> msg => match msg {
                Ok(Request {responder, arguments: count}) => {
                    let _ = responder.send(network_state.external_urls(count));
                },
                _ => {
                    error!(target: "network", "external_urls_receiver closed");
                },
            },
            recv(self.receivers.listened_addresses_receiver) -> msg => match msg {
                Ok(Request {responder, arguments: count}) => {
                    let _ = responder.send(network_state.listened_addresses(count));
                },
                _ => {
                    error!(target: "network", "listened_addresses_receiver closed");
                },
            },
            recv(self.receivers.dial_node_receiver) -> msg => match msg {
                Ok(Request {responder, arguments: (peer_id, addr)}) => {
                    let _ = responder.send(network_state.dial_node(&peer_id, addr));
                },
                _ => {
                    error!(target: "network", "dial_node_receiver closed");
                },
            },
            recv(self.receivers.connected_peers_receiver) -> msg => match msg {
                Ok(Request {responder, arguments}) => {
                    let _ = responder.send(network_state.connected_peers());
                },
                _ => {
                    error!(target: "network", "connected_peers_receiver closed");
                },
            },
            recv(self.receivers.add_discovered_addr_receiver) -> msg => match msg {
                Ok(Request {responder, arguments: (peer_id, addr)}) => {
                    let _ = responder.send(network_state.add_discovered_addr(&peer_id, addr));
                },
                _ => {
                    error!(target: "network", "add_discovered_addr_receiver closed");
                },
            },
            default() => return false,
        }
        true
    }

    fn shutdown(&mut self) {
        let mut network_state = self.network_state.borrow_mut();
        // drop all peers
        network_state.drop_all(&mut self.p2p_control);
    }
}

struct NetworkReceivers {
    external_urls_receiver: Receiver<Request<usize, Vec<(String, u8)>>>,
    listened_addresses_receiver: Receiver<Request<usize, Vec<(Multiaddr, u8)>>>,
    dial_node_receiver: Receiver<Request<(PeerId, Multiaddr), ()>>,
    connected_peers_receiver: Receiver<Request<(), Vec<(PeerId, Peer, MultiaddrList)>>>,
    add_discovered_addr_receiver: Receiver<Request<(PeerId, Multiaddr), ()>>,
}

#[derive(Clone)]
pub struct NetworkController {
    //p2p_control: ServiceControl,
    peer_id: PeerId,
    external_urls_sender: Sender<Request<usize, Vec<(String, u8)>>>,
    listened_addresses_sender: Sender<Request<usize, Vec<(Multiaddr, u8)>>>,
    dial_node_sender: Sender<Request<(PeerId, Multiaddr), ()>>,
    connected_peers_sender: Sender<Request<(), Vec<(PeerId, Peer, MultiaddrList)>>>,
    add_discovered_addr_sender: Sender<Request<(PeerId, Multiaddr), ()>>,
    stop_sender: Sender<Sender<()>>,
}

impl NetworkController {
    pub fn external_urls(&self, max_urls: usize) -> Vec<(String, u8)> {
        Request::call(&self.external_urls_sender, max_urls).expect("external_urls() failed")
    }

    pub fn listened_addresses(&self, count: usize) -> Vec<(Multiaddr, u8)> {
        Request::call(&self.listened_addresses_sender, count).expect("listened_addresses() failed")
    }

    pub fn add_discovered_addr(&self, peer_id: PeerId, addr: Multiaddr) {
        Request::call(&self.add_discovered_addr_sender, (peer_id, addr))
            .expect("add_discovered_addr() failed")
    }

    pub fn local_peer_id(&self) -> &PeerId {
        &self.peer_id
    }

    pub fn node_id(&self) -> String {
        self.peer_id.to_base58()
    }

    /// Send stop signal to network, then wait until network shutdown
    fn shutdown(&mut self) {
        let (stopped_sender, stopped_receiver) = crossbeam_channel::bounded(1);
        self.stop_sender.send(stopped_sender);
        // NOTICE return a disconnect error is in expect, which mean network stream is dropped.
        if let Err(err) = stopped_receiver.recv() {
            debug!(target: "network", "network stopped {:?}", err);
        }
        info!(target: "network", "network shutdown");
    }

    pub fn connected_peers(&self) -> Vec<(PeerId, Peer, MultiaddrList)> {
        Request::call(&self.connected_peers_sender, ()).expect("connected_peers() failed")
    }

    //pub fn with_protocol_context<F, T>(&mut self, protocol_id: ProtocolId, f: F) -> T
    //    where
    //    F: FnOnce(Box<dyn CKBProtocolContext>) -> T,
    //    {
    //        let context = Box::new(DefaultCKBProtocolContext::new(
    //                protocol_id,
    //                self.network_state,
    //                self.p2p_control.clone(),
    //                ));
    //        f(context)
    //    }
}

impl Drop for NetworkController {
    fn drop(&mut self) {
        // FIXME: should gracefully shutdown network in p2p library
        self.shutdown();
    }
}
