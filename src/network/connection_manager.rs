use crate::{
    endpoint::NewConnection, Connecting, Connection, ConnectionOrigin, Endpoint, Incoming, PeerId,
    Request, Response, Result,
};
use bytes::Bytes;
use futures::FutureExt;
use futures::{
    stream::{Fuse, FuturesUnordered},
    StreamExt,
};
use std::{
    collections::{hash_map::Entry, HashMap},
    convert::Infallible,
    net::SocketAddr,
    sync::Arc,
};
use tower::util::BoxCloneService;
use tracing::{error, info};

#[derive(Debug)]
pub enum ConnectionManagerRequest {
    ConnectRequest(SocketAddr, tokio::sync::oneshot::Sender<Result<PeerId>>),
}

struct ConnectingOutput {
    connecting_result: Result<NewConnection>,
    maybe_oneshot: Option<tokio::sync::oneshot::Sender<Result<PeerId>>>,
}

pub struct ConnectionManager {
    endpoint: Arc<Endpoint>,

    mailbox: Fuse<tokio_stream::wrappers::ReceiverStream<ConnectionManagerRequest>>,
    pending_connections: FuturesUnordered<JoinHandle<ConnectingOutput>>,

    active_peers: ActivePeers,
    incoming: Fuse<Incoming>,

    service: BoxCloneService<Request<Bytes>, Response<Bytes>, Infallible>,
}

impl ConnectionManager {
    pub fn new(
        endpoint: Arc<Endpoint>,
        active_peers: ActivePeers,
        incoming: Incoming,
        service: BoxCloneService<Request<Bytes>, Response<Bytes>, Infallible>,
    ) -> (Self, tokio::sync::mpsc::Sender<ConnectionManagerRequest>) {
        let (sender, reciever) = tokio::sync::mpsc::channel(128);
        (
            Self {
                endpoint,
                mailbox: tokio_stream::wrappers::ReceiverStream::new(reciever).fuse(),
                pending_connections: FuturesUnordered::new(),
                active_peers,
                incoming: incoming.fuse(),
                service,
            },
            sender,
        )
    }

    pub async fn start(mut self) {
        info!("ConnectionManager started");

        loop {
            futures::select! {
                request = self.mailbox.select_next_some() => {
                    info!("recieved new request");
                    match request {
                        ConnectionManagerRequest::ConnectRequest(address, oneshot) => {
                            self.handle_connect_request(address, oneshot);
                        }
                    }
                }
                connecting = self.incoming.select_next_some() => {
                    self.handle_incoming(connecting);
                },
                connecting_output = self.pending_connections.select_next_some() => {
                    self.handle_connecting_result(connecting_output);
                },
                complete => break,
            }
        }

        info!("ConnectionManager ended");
    }

    fn add_peer(&mut self, new_connection: NewConnection) {
        if let Some(new_connection) = self
            .active_peers
            .add(&self.endpoint.peer_id(), new_connection)
        {
            let request_handler = super::InboundRequestHandler::new(
                new_connection,
                self.service.clone(),
                self.active_peers.clone(),
            );

            tokio::spawn(request_handler.start());
        }
    }

    fn handle_connect_request(
        &mut self,
        address: SocketAddr,
        oneshot: tokio::sync::oneshot::Sender<Result<PeerId>>,
    ) {
        let connecting = self.endpoint.connect(address);
        let join_handle = JoinHandle(tokio::spawn(async move {
            let connecting_result = match connecting {
                Ok(connecting) => connecting.await,
                Err(e) => Err(e),
            };
            ConnectingOutput {
                connecting_result,
                maybe_oneshot: Some(oneshot),
            }
        }));
        self.pending_connections.push(join_handle);
    }

    fn handle_incoming(&mut self, connecting: Connecting) {
        info!("recieved new incoming connection");
        let join_handle = JoinHandle(tokio::spawn(connecting.map(|connecting_result| {
            ConnectingOutput {
                connecting_result,
                maybe_oneshot: None,
            }
        })));
        self.pending_connections.push(join_handle);
    }

    fn handle_connecting_result(
        &mut self,
        ConnectingOutput {
            connecting_result,
            maybe_oneshot,
        }: ConnectingOutput,
    ) {
        match connecting_result {
            Ok(new_connection) => {
                info!("new connection complete");
                let peer_id = new_connection.connection.peer_id();
                self.add_peer(new_connection);
                if let Some(oneshot) = maybe_oneshot {
                    let _ = oneshot.send(Ok(peer_id));
                }
            }
            Err(e) => {
                error!("inbound connection failed: {e}");
                if let Some(oneshot) = maybe_oneshot {
                    let _ = oneshot.send(Err(e));
                }
            }
        }
    }
}

// JoinHandle that aborts on drop
#[derive(Debug)]
#[must_use]
pub struct JoinHandle<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> std::future::Future for JoinHandle<T> {
    type Output = T;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // If the task panics just propagate it up
        std::pin::Pin::new(&mut self.0).poll(cx).map(Result::unwrap)
    }
}

#[derive(Debug, Clone)]
pub struct ActivePeers(Arc<std::sync::RwLock<ActivePeersInner>>);

impl ActivePeers {
    pub fn new(channel_size: usize) -> Self {
        Self(Arc::new(std::sync::RwLock::new(ActivePeersInner::new(
            channel_size,
        ))))
    }

    #[allow(unused)]
    pub fn subscribe(
        &self,
    ) -> (
        tokio::sync::broadcast::Receiver<crate::types::PeerEvent>,
        Vec<PeerId>,
    ) {
        self.0.read().unwrap().subscribe()
    }

    pub fn peers(&self) -> Vec<PeerId> {
        self.0.read().unwrap().peers()
    }

    pub fn get(&self, peer_id: &PeerId) -> Option<Connection> {
        self.0.read().unwrap().get(peer_id)
    }

    pub fn remove(&self, peer_id: &PeerId, reason: crate::types::DisconnectReason) {
        self.0.write().unwrap().remove(peer_id, reason)
    }

    pub fn remove_with_stable_id(
        &self,
        peer_id: PeerId,
        stable_id: usize,
        reason: crate::types::DisconnectReason,
    ) {
        self.0
            .write()
            .unwrap()
            .remove_with_stable_id(peer_id, stable_id, reason)
    }

    #[must_use]
    fn add(&self, own_peer_id: &PeerId, new_connection: NewConnection) -> Option<NewConnection> {
        self.0.write().unwrap().add(own_peer_id, new_connection)
    }
}

#[derive(Debug)]
pub struct ActivePeersInner {
    connections: HashMap<PeerId, Connection>,
    peer_event_sender: tokio::sync::broadcast::Sender<crate::types::PeerEvent>,
}

impl ActivePeersInner {
    fn new(channel_size: usize) -> Self {
        let (sender, _reciever) = tokio::sync::broadcast::channel(channel_size);
        Self {
            connections: Default::default(),
            peer_event_sender: sender,
        }
    }

    #[allow(unused)]
    fn subscribe(
        &self,
    ) -> (
        tokio::sync::broadcast::Receiver<crate::types::PeerEvent>,
        Vec<PeerId>,
    ) {
        let peers = self.peers();
        let reciever = self.peer_event_sender.subscribe();
        (reciever, peers)
    }

    fn peers(&self) -> Vec<PeerId> {
        self.connections.keys().copied().collect()
    }

    fn get(&self, peer_id: &PeerId) -> Option<Connection> {
        self.connections.get(peer_id).cloned()
    }

    fn remove(&mut self, peer_id: &PeerId, reason: crate::types::DisconnectReason) {
        if let Some(connection) = self.connections.remove(peer_id) {
            // maybe actually provide reason to other side?
            connection.close();

            self.send_event(crate::types::PeerEvent::LostPeer(*peer_id, reason));
        }
    }

    fn remove_with_stable_id(
        &mut self,
        peer_id: PeerId,
        stable_id: usize,
        reason: crate::types::DisconnectReason,
    ) {
        match self.connections.entry(peer_id) {
            Entry::Occupied(entry) => {
                // Only remove the entry if the stable id matches
                if entry.get().stable_id() == stable_id {
                    let (peer_id, connection) = entry.remove_entry();
                    // maybe actually provide reason to other side?
                    connection.close();

                    self.send_event(crate::types::PeerEvent::LostPeer(peer_id, reason));
                }
            }
            Entry::Vacant(_) => {}
        }
    }

    fn send_event(&self, event: crate::types::PeerEvent) {
        // We don't care if anyone is listening
        let _ = self.peer_event_sender.send(event);
    }

    #[must_use]
    fn add(
        &mut self,
        own_peer_id: &PeerId,
        new_connection: NewConnection,
    ) -> Option<NewConnection> {
        // TODO drop Connection if you've somehow connected out ourself

        let peer_id = new_connection.connection.peer_id();
        match self.connections.entry(peer_id) {
            Entry::Occupied(mut entry) => {
                if Self::simultaneous_dial_tie_breaking(
                    own_peer_id,
                    &peer_id,
                    entry.get().origin(),
                    new_connection.connection.origin(),
                ) {
                    info!("closing old connection with {peer_id:?} to mitigate simultaneous dial");
                    let old_connection = entry.insert(new_connection.connection.clone());
                    old_connection.close();
                    self.send_event(crate::types::PeerEvent::LostPeer(
                        peer_id,
                        crate::types::DisconnectReason::Requested,
                    ));
                } else {
                    info!("closing new connection with {peer_id:?} to mitigate simultaneous dial");
                    new_connection.connection.close();
                    // Early return to avoid standing up Incoming Request handlers
                    return None;
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(new_connection.connection.clone());
            }
        }

        self.send_event(crate::types::PeerEvent::NewPeer(peer_id));

        Some(new_connection)
    }

    /// In the event two peers simultaneously dial each other we need to be able to do
    /// tie-breaking to determine which connection to keep and which to drop in a deterministic
    /// way. One simple way is to compare our local PeerId with that of the remote's PeerId and
    /// keep the connection where the peer with the greater PeerId is the dialer.
    ///
    /// Returns `true` if the existing connection should be dropped and `false` if the new
    /// connection should be dropped.
    fn simultaneous_dial_tie_breaking(
        own_peer_id: &PeerId,
        remote_peer_id: &PeerId,
        existing_origin: ConnectionOrigin,
        new_origin: ConnectionOrigin,
    ) -> bool {
        match (existing_origin, new_origin) {
            // If the remote dials while an existing connection is open, the older connection is
            // dropped.
            (ConnectionOrigin::Inbound, ConnectionOrigin::Inbound) => true,
            // We should never dial the same peer twice, but if we do drop the old connection
            (ConnectionOrigin::Outbound, ConnectionOrigin::Outbound) => true,
            (ConnectionOrigin::Inbound, ConnectionOrigin::Outbound) => remote_peer_id < own_peer_id,
            (ConnectionOrigin::Outbound, ConnectionOrigin::Inbound) => own_peer_id < remote_peer_id,
        }
    }
}
