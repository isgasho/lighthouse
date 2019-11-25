use super::manager::SyncMessage;
use crate::service::NetworkMessage;
use beacon_chain::{
    AttestationProcessingOutcome, BeaconChain, BeaconChainTypes, BlockProcessingOutcome,
};
use bls::SignatureSet;
use eth2_libp2p::rpc::methods::*;
use eth2_libp2p::rpc::{RPCEvent, RPCRequest, RPCResponse, RequestId};
use eth2_libp2p::PeerId;
use slog::{debug, error, info, o, trace, warn};
use ssz::Encode;
use state_processing::{
    common::get_indexed_attestation,
    per_block_processing::signature_sets::indexed_attestation_signature_set, per_slot_processing,
};
use std::sync::Arc;
use store::Store;
use tokio::sync::{mpsc, oneshot};
use tree_hash::SignedRoot;
use types::{
    Attestation, BeaconBlock, BeaconState, Domain, Epoch, EthSpec, Hash256, RelativeEpoch, Slot,
};

//TODO: Put a maximum limit on the number of block that can be requested.
//TODO: Rate limit requests

/// If a block is more than `FUTURE_SLOT_TOLERANCE` slots ahead of our slot clock, we drop it.
/// Otherwise we queue it.
pub(crate) const FUTURE_SLOT_TOLERANCE: u64 = 1;

/// Keeps track of syncing information for known connected peers.
#[derive(Clone, Copy, Debug)]
pub struct PeerSyncInfo {
    fork_version: [u8; 4],
    pub finalized_root: Hash256,
    pub finalized_epoch: Epoch,
    pub head_root: Hash256,
    pub head_slot: Slot,
}

impl From<HelloMessage> for PeerSyncInfo {
    fn from(hello: HelloMessage) -> PeerSyncInfo {
        PeerSyncInfo {
            fork_version: hello.fork_version,
            finalized_root: hello.finalized_root,
            finalized_epoch: hello.finalized_epoch,
            head_root: hello.head_root,
            head_slot: hello.head_slot,
        }
    }
}

impl<T: BeaconChainTypes> From<&Arc<BeaconChain<T>>> for PeerSyncInfo {
    fn from(chain: &Arc<BeaconChain<T>>) -> PeerSyncInfo {
        Self::from(hello_message(chain))
    }
}

/// Processes validated messages from the network. It relays necessary data to the syncing thread
/// and processes blocks from the pubsub network.
pub struct MessageProcessor<T: BeaconChainTypes> {
    /// A reference to the underlying beacon chain.
    chain: Arc<BeaconChain<T>>,
    /// A channel to the syncing thread.
    sync_send: mpsc::UnboundedSender<SyncMessage<T::EthSpec>>,
    /// A oneshot channel for destroying the sync thread.
    _sync_exit: oneshot::Sender<()>,
    /// A nextwork context to return and handle RPC requests.
    network: NetworkContext,
    /// The `RPCHandler` logger.
    log: slog::Logger,
}

impl<T: BeaconChainTypes> MessageProcessor<T> {
    /// Instantiate a `MessageProcessor` instance
    pub fn new(
        executor: &tokio::runtime::TaskExecutor,
        beacon_chain: Arc<BeaconChain<T>>,
        network_send: mpsc::UnboundedSender<NetworkMessage>,
        log: &slog::Logger,
    ) -> Self {
        let sync_logger = log.new(o!("service"=> "sync"));
        let sync_network_context = NetworkContext::new(network_send.clone(), sync_logger.clone());

        // spawn the sync thread
        let (sync_send, _sync_exit) = super::manager::spawn(
            executor,
            Arc::downgrade(&beacon_chain),
            sync_network_context,
            sync_logger,
        );

        MessageProcessor {
            chain: beacon_chain,
            sync_send,
            _sync_exit,
            network: NetworkContext::new(network_send, log.clone()),
            log: log.clone(),
        }
    }

    fn send_to_sync(&mut self, message: SyncMessage<T::EthSpec>) {
        self.sync_send.try_send(message).unwrap_or_else(|_| {
            warn!(
                self.log,
                "Could not send message to the sync service";
            )
        });
    }

    /// Handle a peer disconnect.
    ///
    /// Removes the peer from the manager.
    pub fn on_disconnect(&mut self, peer_id: PeerId) {
        self.send_to_sync(SyncMessage::Disconnect(peer_id));
    }

    /// Handle the connection of a new peer.
    ///
    /// Sends a `Hello` message to the peer.
    pub fn on_connect(&mut self, peer_id: PeerId) {
        self.network
            .send_rpc_request(None, peer_id, RPCRequest::Hello(hello_message(&self.chain)));
    }

    /// Handle a `Hello` request.
    ///
    /// Processes the `HelloMessage` from the remote peer and sends back our `Hello`.
    pub fn on_hello_request(
        &mut self,
        peer_id: PeerId,
        request_id: RequestId,
        hello: HelloMessage,
    ) {
        // ignore hello responses if we are shutting down
        trace!(self.log, "HelloRequest"; "peer" => format!("{:?}", peer_id));

        // Say hello back.
        self.network.send_rpc_response(
            peer_id.clone(),
            request_id,
            RPCResponse::Hello(hello_message(&self.chain)),
        );

        self.process_hello(peer_id, hello);
    }

    /// Process a `Hello` response from a peer.
    pub fn on_hello_response(&mut self, peer_id: PeerId, hello: HelloMessage) {
        trace!(self.log, "HelloResponse"; "peer" => format!("{:?}", peer_id));

        // Process the hello message, without sending back another hello.
        self.process_hello(peer_id, hello);
    }

    /// Process a `Hello` message, requesting new blocks if appropriate.
    ///
    /// Disconnects the peer if required.
    fn process_hello(&mut self, peer_id: PeerId, hello: HelloMessage) {
        let remote = PeerSyncInfo::from(hello);
        let local = PeerSyncInfo::from(&self.chain);

        let start_slot = |epoch: Epoch| epoch.start_slot(T::EthSpec::slots_per_epoch());

        if local.fork_version != remote.fork_version {
            // The node is on a different network/fork, disconnect them.
            debug!(
                self.log, "HandshakeFailure";
                "peer" => format!("{:?}", peer_id),
                "reason" => "network_id"
            );

            self.network
                .disconnect(peer_id.clone(), GoodbyeReason::IrrelevantNetwork);
        } else if remote.finalized_epoch <= local.finalized_epoch
            && remote.finalized_root != Hash256::zero()
            && local.finalized_root != Hash256::zero()
            && (self.chain.root_at_slot(start_slot(remote.finalized_epoch))
                != Some(remote.finalized_root))
        {
            // The remotes finalized epoch is less than or greater than ours, but the block root is
            // different to the one in our chain.
            //
            // Therefore, the node is on a different chain and we should not communicate with them.
            debug!(
                self.log, "HandshakeFailure";
                "peer" => format!("{:?}", peer_id),
                "reason" => "different finalized chain"
            );
            self.network
                .disconnect(peer_id.clone(), GoodbyeReason::IrrelevantNetwork);
        } else if remote.finalized_epoch < local.finalized_epoch {
            // The node has a lower finalized epoch, their chain is not useful to us. There are two
            // cases where a node can have a lower finalized epoch:
            //
            // ## The node is on the same chain
            //
            // If a node is on the same chain but has a lower finalized epoch, their head must be
            // lower than ours. Therefore, we have nothing to request from them.
            //
            // ## The node is on a fork
            //
            // If a node is on a fork that has a lower finalized epoch, switching to that fork would
            // cause us to revert a finalized block. This is not permitted, therefore we have no
            // interest in their blocks.
            debug!(
                self.log,
                "NaivePeer";
                "peer" => format!("{:?}", peer_id),
                "reason" => "lower finalized epoch"
            );
        } else if self
            .chain
            .store
            .exists::<BeaconBlock<T::EthSpec>>(&remote.head_root)
            .unwrap_or_else(|_| false)
        {
            trace!(
                self.log, "Peer with known chain found";
                "peer" => format!("{:?}", peer_id),
                "remote_head_slot" => remote.head_slot,
                "remote_latest_finalized_epoch" => remote.finalized_epoch,
            );

            // If the node's best-block is already known to us and they are close to our current
            // head, treat them as a fully sync'd peer.
            self.send_to_sync(SyncMessage::AddPeer(peer_id, remote));
        } else {
            // The remote node has an equal or great finalized epoch and we don't know it's head.
            //
            // Therefore, there are some blocks between the local finalized epoch and the remote
            // head that are worth downloading.
            debug!(
                self.log, "UsefulPeer";
                "peer" => format!("{:?}", peer_id),
                "local_finalized_epoch" => local.finalized_epoch,
                "remote_latest_finalized_epoch" => remote.finalized_epoch,
            );
            self.send_to_sync(SyncMessage::AddPeer(peer_id, remote));
        }
    }

    /// Handle a `RecentBeaconBlocks` request from the peer.
    pub fn on_recent_beacon_blocks_request(
        &mut self,
        peer_id: PeerId,
        request_id: RequestId,
        request: RecentBeaconBlocksRequest,
    ) {
        let blocks: Vec<BeaconBlock<_>> = request
            .block_roots
            .iter()
            .filter_map(|root| {
                if let Ok(Some(block)) = self.chain.store.get::<BeaconBlock<T::EthSpec>>(root) {
                    Some(block)
                } else {
                    debug!(
                        self.log,
                        "Peer requested unknown block";
                        "peer" => format!("{:?}", peer_id),
                        "request_root" => format!("{:}", root),
                    );

                    None
                }
            })
            .collect();

        debug!(
            self.log,
            "RecentBeaconBlocksRequest";
            "peer" => format!("{:?}", peer_id),
            "requested" => request.block_roots.len(),
            "returned" => blocks.len(),
        );

        self.network.send_rpc_response(
            peer_id,
            request_id,
            RPCResponse::BeaconBlocks(blocks.as_ssz_bytes()),
        )
    }

    /// Handle a `BeaconBlocks` request from the peer.
    pub fn on_beacon_blocks_request(
        &mut self,
        peer_id: PeerId,
        request_id: RequestId,
        req: BeaconBlocksRequest,
    ) {
        debug!(
            self.log,
            "BeaconBlocksRequest";
            "peer" => format!("{:?}", peer_id),
            "count" => req.count,
            "start_slot" => req.start_slot,
        );

        //TODO: Optimize this
        // Currently for skipped slots, the blocks returned could be less than the requested range.
        // In the current implementation we read from the db then filter out out-of-range blocks.
        // Improving the db schema to prevent this would be ideal.

        let mut blocks: Vec<BeaconBlock<T::EthSpec>> = self
            .chain
            .rev_iter_block_roots()
            .filter(|(_root, slot)| {
                req.start_slot <= slot.as_u64() && req.start_slot + req.count > slot.as_u64()
            })
            .take_while(|(_root, slot)| req.start_slot <= slot.as_u64())
            .filter_map(|(root, _slot)| {
                if let Ok(Some(block)) = self.chain.store.get::<BeaconBlock<T::EthSpec>>(&root) {
                    Some(block)
                } else {
                    warn!(
                        self.log,
                        "Block in the chain is not in the store";
                        "request_root" => format!("{:}", root),
                    );

                    None
                }
            })
            .filter(|block| block.slot >= req.start_slot)
            .collect();

        blocks.reverse();
        blocks.dedup_by_key(|brs| brs.slot);

        debug!(
            self.log,
            "BeaconBlocksRequest response";
            "peer" => format!("{:?}", peer_id),
            "msg" => "Failed to return all requested hashes",
            "start_slot" => req.start_slot,
            "current_slot" => self.chain.slot().unwrap_or_else(|_| Slot::from(0_u64)).as_u64(),
            "requested" => req.count,
            "returned" => blocks.len(),
        );

        self.network.send_rpc_response(
            peer_id,
            request_id,
            RPCResponse::BeaconBlocks(blocks.as_ssz_bytes()),
        )
    }

    /// Handle a `BeaconBlocks` response from the peer.
    pub fn on_beacon_blocks_response(
        &mut self,
        peer_id: PeerId,
        request_id: RequestId,
        beacon_blocks: Vec<BeaconBlock<T::EthSpec>>,
    ) {
        debug!(
            self.log,
            "BeaconBlocksResponse";
            "peer" => format!("{:?}", peer_id),
            "count" => beacon_blocks.len(),
        );

        self.send_to_sync(SyncMessage::BeaconBlocksResponse {
            peer_id,
            request_id,
            beacon_blocks,
        });
    }

    /// Handle a `RecentBeaconBlocks` response from the peer.
    pub fn on_recent_beacon_blocks_response(
        &mut self,
        peer_id: PeerId,
        request_id: RequestId,
        beacon_blocks: Vec<BeaconBlock<T::EthSpec>>,
    ) {
        debug!(
            self.log,
            "RecentBeaconBlocksResponse";
            "peer" => format!("{:?}", peer_id),
            "count" => beacon_blocks.len(),
        );

        self.send_to_sync(SyncMessage::RecentBeaconBlocksResponse {
            peer_id,
            request_id,
            beacon_blocks,
        });
    }

    /// Process a gossip message declaring a new block.
    ///
    /// Attempts to apply a block to the beacon chain. May queue the block for later processing.
    pub fn on_block_gossip(&mut self, peer_id: PeerId, block: BeaconBlock<T::EthSpec>) {
        match self.chain.process_block(block.clone()) {
            Ok(outcome) => match outcome {
                BlockProcessingOutcome::Processed { .. } => {
                    trace!(self.log, "Gossipsub block processed";
                            "peer_id" => format!("{:?}",peer_id));
                }
                BlockProcessingOutcome::ParentUnknown { parent: _ } => {
                    // Inform the sync manager to find parents for this block
                    trace!(self.log, "Block with unknown parent received";
                            "peer_id" => format!("{:?}",peer_id));
                    self.send_to_sync(SyncMessage::UnknownBlock(peer_id, block.clone()));
                }
                other => {
                    warn!(
                        self.log,
                        "Invalid gossip beacon block";
                        "outcome" => format!("{:?}", other),
                        "block root" => format!("{}", Hash256::from_slice(&block.signed_root()[..])),
                        "block slot" => block.slot
                    );
                    trace!(
                        self.log,
                        "Invalid gossip beacon block ssz";
                        "ssz" => format!("0x{}", hex::encode(block.as_ssz_bytes())),
                    );
                }
            },
            Err(e) => {
                error!(
                    self.log,
                    "Error processing gossip beacon block";
                    "error" => format!("{:?}", e),
                    "block slot" => block.slot
                );
                trace!(
                    self.log,
                    "Erroneous gossip beacon block ssz";
                    "ssz" => format!("0x{}", hex::encode(block.as_ssz_bytes())),
                );
            }
        }
    }

    /// Determines whether or not a given block is fit to be forwarded to other peers.
    pub fn should_forward_block(&mut self, block: BeaconBlock<T::EthSpec>) -> bool {
        // Retrieve the parent block used to generate the signature.
        // This will eventually return false if this operation fails or returns an empty option.
        let parent_block_opt = if let Ok(Some(parent_block)) =
            self.chain
                .store
                .get::<BeaconBlock<T::EthSpec>>(&block.parent_root)
        {
            // Check if the parent block's state root is equal to the current state, if it is, then
            // we can validate the block using the state in our chain head. This saves us from
            // having to make an unecessary database read.
            let state_res = if self.chain.head().beacon_state_root == parent_block.state_root {
                Ok(Some(self.chain.head().beacon_state.clone()))
            } else {
                self.chain
                    .store
                    .get::<BeaconState<T::EthSpec>>(&parent_block.state_root)
            };

            // If we are unable to find a state for the block, we eventually return false. This
            // should never be the case though.
            match state_res {
                Ok(Some(state)) => Some((parent_block, state)),
                _ => None,
            }
        } else {
            None
        };

        // If we found a parent block and state to validate the signature with, we enter this
        // section and find the proposer for the block's slot, otherwise, we return false.
        if let Some((parent_block, mut state)) = parent_block_opt {
            // Determine the epochal relationship between the parent block and the block being verified.
            let relative_epoch = if let Ok(relative_epoch) = RelativeEpoch::from_slot(
                parent_block.slot,
                block.slot,
                T::EthSpec::slots_per_epoch(),
            ) {
                relative_epoch
            } else {
                // This section is entered if the block being verified is too far from the parent to
                // have a RelativeEpoch.

                // We make sure the block being verified follows the parent's slot.
                if state.slot.as_u64() > block.slot.as_u64() {
                    return false;
                }

                // If the block is more than one epoch in the future, we must fast-forward to the
                // state and compute the committee.
                for _ in state.slot.as_u64()..block.slot.as_u64() {
                    if per_slot_processing(&mut state, &self.chain.spec).is_err() {
                        // Return false if something goes wrong.
                        return false;
                    }
                }

                // Compute the committee cache so we can check the proposer.
                // TODO: Downvote peer
                if state
                    .build_committee_cache(RelativeEpoch::Current, &self.chain.spec)
                    .is_err()
                {
                    return false;
                }

                // The relative epoch for the state is now Current.
                RelativeEpoch::Current
            };

            // Compute the proposer for the block's slot.
            let proposer_result = state
                .get_beacon_proposer_index(block.slot, relative_epoch, &self.chain.spec)
                .map(|i| state.validators.get(i));

            // Generate the domain that should have been used to create the signature.
            let domain = self.chain.spec.get_domain(
                block.slot.epoch(T::EthSpec::slots_per_epoch()),
                Domain::BeaconProposer,
                &state.fork,
            );

            // Verify the signature if we were able to get a proposer, otherwise, we eventually
            // return false.
            if let Ok(Some(proposer)) = proposer_result {
                let signature = SignatureSet::single(
                    &block.signature,
                    &proposer.pubkey,
                    block.signed_root(),
                    domain,
                );

                // TODO: Downvote if the signature is invalid.
                return signature.is_valid();
            }
        }

        false
    }

    /// Process a gossip message declaring a new attestation.
    ///
    /// Not currently implemented.
    pub fn on_attestation_gossip(&mut self, _peer_id: PeerId, msg: Attestation<T::EthSpec>) {
        match self.chain.process_attestation(msg.clone()) {
            Ok(outcome) => {
                info!(
                    self.log,
                    "Processed attestation";
                    "source" => "gossip",
                    "outcome" => format!("{:?}", outcome)
                );

                if outcome != AttestationProcessingOutcome::Processed {
                    trace!(
                        self.log,
                        "Invalid gossip attestation ssz";
                        "ssz" => format!("0x{}", hex::encode(msg.as_ssz_bytes())),
                    );
                }
            }
            Err(e) => {
                trace!(
                    self.log,
                    "Erroneous gossip attestation ssz";
                    "ssz" => format!("0x{}", hex::encode(msg.as_ssz_bytes())),
                );
                error!(self.log, "Invalid gossip attestation"; "error" => format!("{:?}", e));
            }
        };
    }

    /// Determines whether or not a given attestation is fit to be forwarded to other peers.
    pub fn should_forward_attestation(&self, attestation: Attestation<T::EthSpec>) -> bool {
        // Attempt to validate the attestation's signature against the head state.
        // In this case, we do not read anything from the database, which should be fast and will
        // work for most attestations that get passed around the network.
        let head_state = &self.chain.head().beacon_state;

        // Convert the attestation to an indexed attestation.
        if let Ok(indexed_attestation) = get_indexed_attestation(&head_state, &attestation) {
            // Validate the signature and return true if it is valid. Otherwise, we move on and read
            // the database to make certain we have the correct state.
            if let Ok(signature) = indexed_attestation_signature_set(
                &head_state,
                &indexed_attestation.signature,
                &indexed_attestation,
                &self.chain.spec,
            ) {
                // An invalid signature here does not necessarily mean the attestation is invalid.
                // It could be the case that our state has a different validator registry.
                if signature.is_valid() {
                    return true;
                }
            }
        }

        // If the first check did not pass, we retrieve the block for the beacon_block_root in the
        // attestation's data and use that to check the signature.
        if let Ok(Some(block)) = self
            .chain
            .store
            .get::<BeaconBlock<T::EthSpec>>(&attestation.data.beacon_block_root)
        {
            // Retrieve the block's state.
            if let Ok(Some(state)) = self
                .chain
                .store
                .get::<BeaconState<T::EthSpec>>(&block.state_root)
            {
                // Convert the attestation to an indexed attestation.
                if let Ok(indexed_attestation) = get_indexed_attestation(&state, &attestation) {
                    // Check if the signature is valid against the state we got from the database.
                    if let Ok(signature) = indexed_attestation_signature_set(
                        &state,
                        &indexed_attestation.signature,
                        &indexed_attestation,
                        &self.chain.spec,
                    ) {
                        // TODO: Maybe downvote peer if the signature is invalid.
                        return signature.is_valid();
                    }
                }
            }
        }

        false
    }
}

/// Build a `HelloMessage` representing the state of the given `beacon_chain`.
pub(crate) fn hello_message<T: BeaconChainTypes>(beacon_chain: &BeaconChain<T>) -> HelloMessage {
    let state = &beacon_chain.head().beacon_state;

    HelloMessage {
        fork_version: state.fork.current_version,
        finalized_root: state.finalized_checkpoint.root,
        finalized_epoch: state.finalized_checkpoint.epoch,
        head_root: beacon_chain.head().beacon_block_root,
        head_slot: state.slot,
    }
}

/// Wraps a Network Channel to employ various RPC/Sync related network functionality.
pub struct NetworkContext {
    /// The network channel to relay messages to the Network service.
    network_send: mpsc::UnboundedSender<NetworkMessage>,
    /// Logger for the `NetworkContext`.
    log: slog::Logger,
}

impl NetworkContext {
    pub fn new(network_send: mpsc::UnboundedSender<NetworkMessage>, log: slog::Logger) -> Self {
        Self { network_send, log }
    }

    pub fn disconnect(&mut self, peer_id: PeerId, reason: GoodbyeReason) {
        warn!(
            &self.log,
            "Disconnecting peer (RPC)";
            "reason" => format!("{:?}", reason),
            "peer_id" => format!("{:?}", peer_id),
        );
        self.send_rpc_request(None, peer_id, RPCRequest::Goodbye(reason))
        // TODO: disconnect peers.
    }

    pub fn send_rpc_request(
        &mut self,
        request_id: Option<RequestId>,
        peer_id: PeerId,
        rpc_request: RPCRequest,
    ) {
        // use 0 as the default request id, when an ID is not required.
        let request_id = request_id.unwrap_or_else(|| 0);
        self.send_rpc_event(peer_id, RPCEvent::Request(request_id, rpc_request));
    }

    //TODO: Handle Error responses
    pub fn send_rpc_response(
        &mut self,
        peer_id: PeerId,
        request_id: RequestId,
        rpc_response: RPCResponse,
    ) {
        self.send_rpc_event(
            peer_id,
            RPCEvent::Response(request_id, RPCErrorResponse::Success(rpc_response)),
        );
    }

    fn send_rpc_event(&mut self, peer_id: PeerId, rpc_event: RPCEvent) {
        self.network_send
            .try_send(NetworkMessage::RPC(peer_id, rpc_event))
            .unwrap_or_else(|_| {
                warn!(
                    self.log,
                    "Could not send RPC message to the network service"
                )
            });
    }
}
