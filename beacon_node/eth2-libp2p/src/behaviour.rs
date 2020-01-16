use crate::discovery::Discovery;
use crate::rpc::{RPCEvent, RPCMessage, RPC};
use crate::types::error;
use crate::NetworkConfig;
use crate::PubsubMessage;
use crate::{Topic, TopicHash};
use futures::prelude::*;
use libp2p::{
    core::identity::Keypair,
    discv5::Discv5Event,
    gossipsub::{Gossipsub, GossipsubEvent, GossipsubMessage, MessageId},
    identify::{Identify, IdentifyEvent},
    swarm::{NetworkBehaviourAction, NetworkBehaviourEventProcess},
    tokio_io::{AsyncRead, AsyncWrite},
    NetworkBehaviour, PeerId,
};
use lru::LruCache;
use slog::{debug, o, warn};
use types::EthSpec;

const MAX_IDENTIFY_ADDRESSES: usize = 20;

/// Builds the network behaviour that manages the core protocols of eth2.
/// This core behaviour is managed by `Behaviour` which adds peer management to all core
/// behaviours.
#[derive(NetworkBehaviour)]
#[behaviour(out_event = "BehaviourEvent<TSpec>", poll_method = "poll")]
pub struct Behaviour<TSubstream: AsyncRead + AsyncWrite, TSpec: EthSpec> {
    /// The routing pub-sub mechanism for eth2.
    gossipsub: Gossipsub<TSubstream>,
    /// The Eth2 RPC specified in the wire-0 protocol.
    eth2_rpc: RPC<TSubstream>,
    /// Keep regular connection to peers and disconnect if absent.
    // TODO: Using id for initial interop. This will be removed by mainnet.
    /// Provides IP addresses and peer information.
    identify: Identify<TSubstream>,
    /// Discovery behaviour.
    discovery: Discovery<TSubstream>,
    /// The events generated by this behaviour to be consumed in the swarm poll.
    #[behaviour(ignore)]
    events: Vec<BehaviourEvent<TSpec>>,
    /// A cache of recently seen gossip messages. This is used to filter out any possible
    /// duplicates that may still be seen over gossipsub.
    #[behaviour(ignore)]
    seen_gossip_messages: LruCache<GossipsubMessage, ()>,
    /// Logger for behaviour actions.
    #[behaviour(ignore)]
    log: slog::Logger,
}

impl<TSubstream: AsyncRead + AsyncWrite, TSpec: EthSpec> Behaviour<TSubstream, TSpec> {
    pub fn new(
        local_key: &Keypair,
        net_conf: &NetworkConfig,
        log: &slog::Logger,
    ) -> error::Result<Self> {
        let local_peer_id = local_key.public().clone().into_peer_id();
        let behaviour_log = log.new(o!());

        let identify = Identify::new(
            "lighthouse/libp2p".into(),
            version::version(),
            local_key.public(),
        );

        Ok(Behaviour {
            eth2_rpc: RPC::new(log.clone()),
            gossipsub: Gossipsub::new(local_peer_id.clone(), net_conf.gs_config.clone()),
            discovery: Discovery::new(local_key, net_conf, log)?,
            identify,
            seen_gossip_messages: LruCache::new(256),
            events: Vec::new(),
            log: behaviour_log,
        })
    }

    pub fn discovery(&self) -> &Discovery<TSubstream> {
        &self.discovery
    }

    pub fn gs(&self) -> &Gossipsub<TSubstream> {
        &self.gossipsub
    }
}

// Implement the NetworkBehaviourEventProcess trait so that we can derive NetworkBehaviour for Behaviour
impl<TSubstream: AsyncRead + AsyncWrite, TSpec: EthSpec>
    NetworkBehaviourEventProcess<GossipsubEvent> for Behaviour<TSubstream, TSpec>
{
    fn inject_event(&mut self, event: GossipsubEvent) {
        match event {
            GossipsubEvent::Message(propagation_source, id, gs_msg) => {
                // Note: We are keeping track here of the peer that sent us the message, not the
                // peer that originally published the message.
                if self.seen_gossip_messages.put(gs_msg.clone(), ()).is_none() {
                    match PubsubMessage::decode(&gs_msg.topics, gs_msg.data) {
                        Err(e) => debug!(self.log, "Could not decode gossipsub message: {}", e);
                        Ok(msg) => {
                            // if this message isn't a duplicate, notify the network
                            self.events.push(BehaviourEvent::GossipMessage {
                                id,
                                source: propagation_source,
                                topics: gs_msg.topics,
                                message: msg,
                            });
                        }
                    }
                } else {
                    warn!(self.log, "A duplicate gossipsub message was received"; "message" => format!("{:?}", msg));
                }
            }
            GossipsubEvent::Subscribed { peer_id, topic } => {
                self.events
                    .push(BehaviourEvent::PeerSubscribed(peer_id, topic));
            }
            GossipsubEvent::Unsubscribed { .. } => {}
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite, TSpec: EthSpec> NetworkBehaviourEventProcess<RPCMessage>
    for Behaviour<TSubstream, TSpec>
{
    fn inject_event(&mut self, event: RPCMessage) {
        match event {
            RPCMessage::PeerDialed(peer_id) => {
                self.events.push(BehaviourEvent::PeerDialed(peer_id))
            }
            RPCMessage::PeerDisconnected(peer_id) => {
                self.events.push(BehaviourEvent::PeerDisconnected(peer_id))
            }
            RPCMessage::RPC(peer_id, rpc_event) => {
                self.events.push(BehaviourEvent::RPC(peer_id, rpc_event))
            }
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite, TSpec: EthSpec> Behaviour<TSubstream, TSpec> {
    /// Consumes the events list when polled.
    fn poll<TBehaviourIn>(
        &mut self,
    ) -> Async<NetworkBehaviourAction<TBehaviourIn, BehaviourEvent<TSpec>>> {
        if !self.events.is_empty() {
            return Async::Ready(NetworkBehaviourAction::GenerateEvent(self.events.remove(0)));
        }

        Async::NotReady
    }
}

impl<TSubstream: AsyncRead + AsyncWrite, TSpec: EthSpec> NetworkBehaviourEventProcess<IdentifyEvent>
    for Behaviour<TSubstream, TSpec>
{
    fn inject_event(&mut self, event: IdentifyEvent) {
        match event {
            IdentifyEvent::Received {
                peer_id,
                mut info,
                observed_addr,
            } => {
                if info.listen_addrs.len() > MAX_IDENTIFY_ADDRESSES {
                    debug!(
                        self.log,
                        "More than 20 addresses have been identified, truncating"
                    );
                    info.listen_addrs.truncate(MAX_IDENTIFY_ADDRESSES);
                }
                debug!(self.log, "Identified Peer"; "peer" => format!("{}", peer_id),
                "protocol_version" => info.protocol_version,
                "agent_version" => info.agent_version,
                "listening_ addresses" => format!("{:?}", info.listen_addrs),
                "observed_address" => format!("{:?}", observed_addr),
                "protocols" => format!("{:?}", info.protocols)
                );
            }
            IdentifyEvent::Sent { .. } => {}
            IdentifyEvent::Error { .. } => {}
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite, TSpec: EthSpec> NetworkBehaviourEventProcess<Discv5Event>
    for Behaviour<TSubstream, TSpec>
{
    fn inject_event(&mut self, _event: Discv5Event) {
        // discv5 has no events to inject
    }
}

/// Implements the combined behaviour for the libp2p service.
impl<TSubstream: AsyncRead + AsyncWrite, TSpec: EthSpec> Behaviour<TSubstream, TSpec> {
    /* Pubsub behaviour functions */

    /// Subscribes to a gossipsub topic.
    pub fn subscribe(&mut self, topic: Topic) -> bool {
        self.gossipsub.subscribe(topic)
    }

    /// Unsubscribe from a gossipsub topic.
    pub fn unsubscribe(&mut self, topic: Topic) -> bool {
        self.gossipsub.unsubscribe(topic)
    }

    /// Publishes a message on the pubsub (gossipsub) behaviour.
    pub fn publish(&mut self, topics: &[Topic], message: PubsubMessage<TSpec>) {
        let message_data = message.encode(topics);
        for topic in topics {
            self.gossipsub.publish(topic, message_data.clone());
        }
    }

    /// Forwards a message that is waiting in gossipsub's mcache. Messages are only propagated
    /// once validated by the beacon chain.
    pub fn propagate_message(&mut self, propagation_source: &PeerId, message_id: MessageId) {
        self.gossipsub
            .propagate_message(&message_id, propagation_source);
    }

    /* Eth2 RPC behaviour functions */

    /// Sends an RPC Request/Response via the RPC protocol.
    pub fn send_rpc(&mut self, peer_id: PeerId, rpc_event: RPCEvent) {
        self.eth2_rpc.send_rpc(peer_id, rpc_event);
    }

    /* Discovery / Peer management functions */
    /// Return the list of currently connected peers.
    pub fn connected_peers(&self) -> usize {
        self.discovery.connected_peers()
    }

    /// Notify discovery that the peer has been banned.
    pub fn peer_banned(&mut self, peer_id: PeerId) {
        self.discovery.peer_banned(peer_id);
    }

    /// Notify discovery that the peer has been unbanned.
    pub fn peer_unbanned(&mut self, peer_id: &PeerId) {
        self.discovery.peer_unbanned(peer_id);
    }
}

/// The types of events than can be obtained from polling the behaviour.
pub enum BehaviourEvent<TSpec: EthSpec> {
    /// A received RPC event and the peer that it was received from.
    RPC(PeerId, RPCEvent),
    /// We have completed an initial connection to a new peer.
    PeerDialed(PeerId),
    /// A peer has disconnected.
    PeerDisconnected(PeerId),
    /// A gossipsub message has been received.
    GossipMessage {
        /// The gossipsub message id. Used when propagating blocks after validation.
        id: MessageId,
        /// The peer from which we received this message, not the peer that published it.
        source: PeerId,
        /// The topics that this message was sent on.
        topics: Vec<TopicHash>,
        /// The message itself.
        message: PubsubMessage<TSpec>,
    },
    /// Subscribed to peer for given topic
    PeerSubscribed(PeerId, TopicHash),
}
