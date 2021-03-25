use crate::codec::*;
use crate::endpoint::Endpoint;
use crate::error::*;
use crate::fair_queue::{FairQueue, QueueInner};
use crate::transport::AcceptStopHandle;
use crate::*;
use crate::{SocketType, ZmqResult};
use async_trait::async_trait;
use dashmap::DashMap;
use futures::SinkExt;
use futures::StreamExt;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

struct RepPeer {
    pub(crate) _identity: PeerIdentity,
    pub(crate) send_queue: ZmqFramedWrite,
}

struct RepSocketBackend {
    pub(crate) peers: DashMap<PeerIdentity, RepPeer>,
    fair_queue_inner: Arc<Mutex<QueueInner<ZmqFramedRead, PeerIdentity>>>,
    socket_monitor: Mutex<Option<mpsc::Sender<SocketEvent>>>,
}

pub struct RepSocket {
    backend: Arc<RepSocketBackend>,
    envelope: Option<ZmqMessage>,
    current_request: Option<PeerIdentity>,
    fair_queue: FairQueue<ZmqFramedRead, PeerIdentity>,
    binds: HashMap<Endpoint, AcceptStopHandle>,
}

impl Drop for RepSocket {
    fn drop(&mut self) {
        self.backend.shutdown();
    }
}

#[async_trait]
impl Socket for RepSocket {
    fn new() -> Self {
        let fair_queue = FairQueue::new(true);
        Self {
            backend: Arc::new(RepSocketBackend {
                peers: DashMap::new(),
                fair_queue_inner: fair_queue.inner(),
                socket_monitor: Mutex::new(None),
            }),
            envelope: None,
            current_request: None,
            fair_queue,
            binds: HashMap::new(),
        }
    }

    fn backend(&self) -> Arc<dyn MultiPeerBackend> {
        self.backend.clone()
    }

    fn binds(&mut self) -> &mut HashMap<Endpoint, AcceptStopHandle> {
        &mut self.binds
    }

    fn monitor(&mut self) -> mpsc::Receiver<SocketEvent> {
        let (sender, receiver) = mpsc::channel(1024);
        self.backend.socket_monitor.lock().replace(sender);
        receiver
    }
}

impl MultiPeerBackend for RepSocketBackend {
    fn peer_connected(self: Arc<Self>, peer_id: &PeerIdentity, io: FramedIo) {
        let (recv_queue, send_queue) = io.into_parts();

        self.peers.insert(
            peer_id.clone(),
            RepPeer {
                _identity: peer_id.clone(),
                send_queue,
            },
        );
        self.fair_queue_inner
            .lock()
            .insert(peer_id.clone(), recv_queue);
    }

    fn peer_disconnected(&self, peer_id: &PeerIdentity) {
        if let Some(monitor) = self.monitor().lock().as_mut() {
            let _ = monitor.try_send(SocketEvent::Disconnected(peer_id.clone()));
        }
        self.peers.remove(peer_id);
    }
}

impl SocketBackend for RepSocketBackend {
    fn socket_type(&self) -> SocketType {
        SocketType::REP
    }

    fn shutdown(&self) {
        self.peers.clear();
    }

    fn monitor(&self) -> &Mutex<Option<mpsc::Sender<SocketEvent>>> {
        &self.socket_monitor
    }
}

#[async_trait]
impl SocketSend for RepSocket {
    async fn send(&mut self, mut message: ZmqMessage) -> ZmqResult<()> {
        match self.current_request.take() {
            Some(peer_id) => {
                if let Some(mut peer) = self.backend.peers.get_mut(&peer_id) {
                    if let Some(envelope) = self.envelope.take() {
                        message.prepend(&envelope);
                    }
                    peer.send_queue.send(Message::Message(message)).await?;
                    Ok(())
                } else {
                    Err(ZmqError::ReturnToSender {
                        reason: "Client disconnected",
                        message,
                    })
                }
            }
            None => Err(ZmqError::ReturnToSender {
                reason: "Unable to send reply. No request in progress",
                message,
            }),
        }
    }
}

#[async_trait]
impl SocketRecv for RepSocket {
    async fn recv(&mut self) -> ZmqResult<ZmqMessage> {
        loop {
            match self.fair_queue.next().await {
                Some((peer_id, Ok(message))) => match message {
                    Message::Message(mut m) => {
                        assert!(m.len() > 1);
                        let mut at = 1;
                        for (index, frame) in m.iter().enumerate() {
                            if frame.is_empty() {
                                // Include delimiter in envelope.
                                at = index + 1;
                                break;
                            }
                        }
                        let data = m.split_off(at);
                        self.envelope = Some(m);
                        self.current_request = Some(peer_id);
                        return Ok(data);
                    }
                    _ => todo!(),
                },
                Some((_peer_id, _)) => todo!(),
                None => return Err(ZmqError::NoMessage),
            };
        }
    }
}
