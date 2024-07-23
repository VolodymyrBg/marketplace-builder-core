use anyhow::Context;
use hotshot::{
    traits::{election::static_committee::GeneralStaticCommittee, NodeImplementation},
    types::{Event, SystemContextHandle},
};
use hotshot_builder_api::v0_3::{
    block_info::{AvailableBlockData, AvailableBlockInfo},
    builder::BuildError,
    data_source::{AcceptsTxnSubmits, BuilderDataSource},
};
use hotshot_types::{
    data::{DaProposal, Leaf, QuorumProposal, ViewNumber},
    event::EventType,
    message::Proposal,
    traits::{
        block_contents::BlockPayload,
        consensus_api::ConsensusApi,
        election::Membership,
        node_implementation::{ConsensusTime, NodeType},
        signature_key::{BuilderSignatureKey, SignatureKey},
    },
    utils::BuilderCommitment,
    vid::VidCommitment,
};

use std::fmt::Debug;
use std::hash::Hash;

use crate::{
    bid::{from_bid_config, BidConfig},
    builder_state::{
        BuildBlockInfo, DaProposalMessage, DecideMessage, MessageType, QCMessage, RequestMessage,
        ResponseMessage, TransactionSource,
    },
};
use anyhow::anyhow;
use async_broadcast::Sender as BroadcastSender;
pub use async_broadcast::{broadcast, RecvError, TryRecvError};
use async_compatibility_layer::{art::async_sleep, art::async_timeout, channel::unbounded};
use async_lock::RwLock;
use async_trait::async_trait;
use committable::{Commitment, Committable};
use derivative::Derivative;
use futures::future::BoxFuture;
use futures::stream::StreamExt;
use hotshot_events_service::{events::Error as EventStreamError, events_source::StartupInfo};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use std::{fmt::Display, time::Instant};
use surf_disco::Client;
use tagged_base64::TaggedBase64;
use tide_disco::{method::ReadState, Url};

/// BuilderTransaction is a trait that allows the builder to retrieve transaction namespace ids
/// to filter for transactions that are relevant to the builder.
pub trait BuilderTransaction {
    /// Type representing the namespace id for a transaction.
    type NamespaceId: Clone
        + Copy
        + Serialize
        + DeserializeOwned
        + Debug
        + Send
        + Sync
        + PartialEq
        + Eq
        + Hash;

    fn namespace_id(&self) -> Self::NamespaceId;
}

// It holds all the necessary information for a block
#[derive(Debug)]
pub struct BlockInfo<TYPES: NodeType>
where
    TYPES::Transaction: BuilderTransaction,
{
    pub block_payload: TYPES::BlockPayload,
    pub metadata: <<TYPES as NodeType>::BlockPayload as BlockPayload<TYPES>>::Metadata,
    pub offered_fee: u64,
}

// It holds the information for the proposed block
#[derive(Debug)]
pub struct ProposedBlockId<TYPES: NodeType> {
    pub parent_commitment: VidCommitment,
    pub payload_commitment: BuilderCommitment,
    pub parent_view: TYPES::Time,
}

impl<TYPES: NodeType> ProposedBlockId<TYPES> {
    pub fn new(
        parent_commitment: VidCommitment,
        payload_commitment: BuilderCommitment,
        parent_view: TYPES::Time,
    ) -> Self {
        ProposedBlockId {
            parent_commitment,
            payload_commitment,
            parent_view,
        }
    }
}

#[derive(Debug, Derivative)]
#[derivative(Default(bound = ""))]
pub struct BuilderStatesInfo<TYPES: NodeType> {
    // list of all the builder states spawned for a view
    pub vid_commitments: Vec<VidCommitment>,
    // list of all the proposed blocks for a view
    pub block_ids: Vec<ProposedBlockId<TYPES>>,
}

#[derive(Debug)]
pub struct ReceivedTransaction<TYPES: NodeType> {
    // the transaction
    pub tx: TYPES::Transaction,
    // its hash
    pub commit: Commitment<TYPES::Transaction>,
    // its source
    pub source: TransactionSource,
    // received time
    pub time_in: Instant,
}

#[allow(clippy::type_complexity)]
#[derive(Debug)]
pub struct GlobalState<TYPES: NodeType>
where
    TYPES::Transaction: BuilderTransaction,
{
    /// id of namespace builder is building for. None if the builder builds for all namespaces
    pub namespace_id: Option<<TYPES::Transaction as BuilderTransaction>::NamespaceId>,

    // data store for the blocks
    pub block_hash_to_block: HashMap<(BuilderCommitment, TYPES::Time), BlockInfo<TYPES>>,

    // registered builder states
    pub spawned_builder_states:
        HashMap<(VidCommitment, TYPES::Time), BroadcastSender<MessageType<TYPES>>>,

    // builder state -> last built block , it is used to respond the client
    // if the req channel times out during get_available_blocks
    pub builder_state_to_last_built_block: HashMap<(VidCommitment, TYPES::Time), ResponseMessage>,

    // // scheduled GC by view number
    // pub view_to_cleanup_targets: BTreeMap<TYPES::Time, BuilderStatesInfo<TYPES>>,

    // sending a transaction from the hotshot/private mempool to the builder states
    // NOTE: Currently, we don't differentiate between the transactions from the hotshot and the private mempool
    pub tx_sender: BroadcastSender<Arc<ReceivedTransaction<TYPES>>>,

    // last garbage collected view number
    pub last_garbage_collected_view_num: TYPES::Time,

    // highest view running builder task
    pub highest_view_num_builder_id: (VidCommitment, TYPES::Time),
}

impl<TYPES: NodeType> GlobalState<TYPES>
where
    TYPES::Transaction: BuilderTransaction,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        namespace_id: Option<<TYPES::Transaction as BuilderTransaction>::NamespaceId>,
        bootstrap_sender: BroadcastSender<MessageType<TYPES>>,
        tx_sender: BroadcastSender<Arc<ReceivedTransaction<TYPES>>>,
        bootstrapped_builder_state_id: VidCommitment,
        bootstrapped_view_num: TYPES::Time,
        last_garbage_collected_view_num: TYPES::Time,
        _buffer_view_num_count: u64,
    ) -> Self {
        let mut spawned_builder_states = HashMap::new();
        spawned_builder_states.insert(
            (bootstrapped_builder_state_id, bootstrapped_view_num),
            bootstrap_sender.clone(),
        );
        GlobalState {
            block_hash_to_block: Default::default(),
            spawned_builder_states,
            namespace_id,
            tx_sender,
            last_garbage_collected_view_num,
            builder_state_to_last_built_block: Default::default(),
            highest_view_num_builder_id: (bootstrapped_builder_state_id, bootstrapped_view_num),
        }
    }

    pub fn register_builder_state(
        &mut self,
        vid_commmit: VidCommitment,
        view_num: TYPES::Time,
        request_sender: BroadcastSender<MessageType<TYPES>>,
    ) {
        // register the builder state
        self.spawned_builder_states
            .insert((vid_commmit, view_num), request_sender);

        // keep track of the max view number
        if view_num > self.highest_view_num_builder_id.1 {
            tracing::info!(
                "registering builder {:?}@{:?} as highest",
                vid_commmit,
                view_num
            );
            self.highest_view_num_builder_id = (vid_commmit, view_num);
        } else {
            tracing::warn!(
                "builder {:?}@{:?} created; highest registered is {:?}@{:?}",
                vid_commmit,
                view_num,
                self.highest_view_num_builder_id.0,
                self.highest_view_num_builder_id.1
            );
        }
    }

    pub fn update_global_state(
        &mut self,
        build_block_info: BuildBlockInfo<TYPES>,
        builder_vid_commitment: VidCommitment,
        view_num: TYPES::Time,
        response_msg: ResponseMessage,
    ) {
        self.block_hash_to_block
            .entry((build_block_info.builder_hash, view_num))
            .or_insert_with(|| BlockInfo {
                block_payload: build_block_info.block_payload,
                metadata: build_block_info.metadata,
                offered_fee: build_block_info.offered_fee,
            });

        // update the builder state to last built block
        self.builder_state_to_last_built_block
            .insert((builder_vid_commitment, view_num), response_msg);
    }

    // remove the builder state handles based on the decide event
    pub fn remove_handles(&mut self, on_decide_view: TYPES::Time) -> TYPES::Time {
        // remove everything from the spawned builder states when view_num <= on_decide_view;
        // if we don't have a highest view > decide, use highest view as cutoff.
        let cutoff = std::cmp::min(self.highest_view_num_builder_id.1, on_decide_view);
        self.spawned_builder_states
            .retain(|(_vid, view_num), _channel| *view_num >= cutoff);

        let cutoff_u64 = cutoff.u64();
        let gc_view = if cutoff_u64 > 0 { cutoff_u64 - 1 } else { 0 };

        self.last_garbage_collected_view_num = TYPES::Time::new(gc_view);

        cutoff
    }

    // private mempool submit txn
    // Currently, we don't differentiate between the transactions from the hotshot and the private mempool
    pub async fn submit_client_txns(
        &self,
        txns: Vec<<TYPES as NodeType>::Transaction>,
    ) -> Result<Vec<Commitment<<TYPES as NodeType>::Transaction>>, BuildError> {
        handle_received_txns(
            &self.tx_sender,
            txns,
            TransactionSource::External,
            self.namespace_id,
        )
        .await
    }

    pub fn get_channel_for_matching_builder_or_highest_view_buider(
        &self,
        key: &(VidCommitment, TYPES::Time),
    ) -> Result<&BroadcastSender<MessageType<TYPES>>, BuildError> {
        if let Some(channel) = self.spawned_builder_states.get(key) {
            tracing::info!("Got matching builder for parent {:?}@{:?}", key.0, key.1);
            Ok(channel)
        } else {
            tracing::warn!(
                "failed to recover builder for parent {:?}@{:?}, using higest view num builder with {:?}@{:?}",
                key.0,
                key.1,
                self.highest_view_num_builder_id.0,
                self.highest_view_num_builder_id.1
            );
            // get the sender for the highest view number builder
            self.spawned_builder_states
                .get(&self.highest_view_num_builder_id)
                .ok_or_else(|| BuildError::Error {
                    message: "No builder state found".to_string(),
                })
        }
    }

    // check for the existence of the builder state for a view
    pub fn check_builder_state_existence_for_a_view(&self, key: &TYPES::Time) -> bool {
        // iterate over the spawned builder states and check if the view number exists
        self.spawned_builder_states
            .iter()
            .any(|((_vid, view_num), _sender)| view_num == key)
    }

    pub fn should_view_handle_other_proposals(
        &self,
        builder_view: &TYPES::Time,
        proposal_view: &TYPES::Time,
    ) -> bool {
        *builder_view == self.highest_view_num_builder_id.1
            && !self.check_builder_state_existence_for_a_view(proposal_view)
    }
}

pub struct ProxyGlobalState<TYPES: NodeType>
where
    TYPES::Transaction: BuilderTransaction,
{
    // global state
    global_state: Arc<RwLock<GlobalState<TYPES>>>,

    // identity keys for the builder
    // May be ideal place as GlobalState interacts with hotshot apis
    // and then can sign on responders as desired
    builder_keys: (
        TYPES::BuilderSignatureKey, // pub key
        <<TYPES as NodeType>::BuilderSignatureKey as BuilderSignatureKey>::BuilderPrivateKey, // private key
    ),

    // max waiting time to serve first api request
    max_api_waiting_time: Duration,
}

impl<TYPES: NodeType> ProxyGlobalState<TYPES>
where
    TYPES::Transaction: BuilderTransaction,
{
    pub fn new(
        global_state: Arc<RwLock<GlobalState<TYPES>>>,
        builder_keys: (
            TYPES::BuilderSignatureKey,
            <<TYPES as NodeType>::BuilderSignatureKey as BuilderSignatureKey>::BuilderPrivateKey,
        ),
        max_api_waiting_time: Duration,
    ) -> Self {
        ProxyGlobalState {
            global_state,
            builder_keys,
            max_api_waiting_time,
        }
    }
}

/*
Handling Builder API responses
*/
#[async_trait]
impl<TYPES: NodeType> BuilderDataSource<TYPES> for ProxyGlobalState<TYPES>
where
    TYPES::Transaction: BuilderTransaction,
    for<'a> <<TYPES::SignatureKey as SignatureKey>::PureAssembledSignatureType as TryFrom<
        &'a TaggedBase64,
    >>::Error: Display,
    for<'a> <TYPES::SignatureKey as TryFrom<&'a TaggedBase64>>::Error: Display,
{
    async fn available_blocks(
        &self,
        for_parent: &VidCommitment,
        view_number: u64,
        sender: TYPES::SignatureKey,
        signature: &<TYPES::SignatureKey as SignatureKey>::PureAssembledSignatureType,
    ) -> Result<Vec<AvailableBlockInfo<TYPES>>, BuildError> {
        let starting_time = Instant::now();

        // verify the signature
        if !sender.validate(signature, for_parent.as_ref()) {
            tracing::error!("Signature validation failed in get_available_blocks");
            return Err(BuildError::Error {
                message: "Signature validation failed in get_available_blocks".to_string(),
            });
        }

        tracing::info!(
            "Requesting available blocks for (parent {:?}, view_num: {:?})",
            for_parent,
            view_number
        );

        let view_num = <<TYPES as NodeType>::Time as ConsensusTime>::new(view_number);
        // check in the local spawned builder states
        // if it doesn't exist; there are three cases
        // 1) it has already been garbage collected (view < decide) and we should return an error
        // 2) it has not yet been created, and we should try to wait
        // 3) we missed the triggering event, and should use the BuilderState with the highest available view

        {
            // 1st case: Decide event received, and not bootstrapping.
            // If this `BlockBuilder` hasn't been reaped, it should have been.
            let global_state = self.global_state.read_arc().await;
            if view_num < global_state.last_garbage_collected_view_num
                && global_state.highest_view_num_builder_id.1
                    != global_state.last_garbage_collected_view_num
            {
                tracing::warn!(
                    "Requesting for view {:?}, last decide-triggered cleanup on view {:?}, highest view num is {:?}",
                    view_num,
                    global_state.last_garbage_collected_view_num,
                    global_state.highest_view_num_builder_id.1
                );
                return Err(BuildError::Error {
                    message:
                        "Request for available blocks for a view that has already been decided."
                            .to_string(),
                });
            }
        }

        let (response_sender, response_receiver) = unbounded();
        let req_msg = RequestMessage {
            requested_vid_commitment: (*for_parent),
            requested_view_number: view_number,
            response_channel: response_sender,
        };
        let timeout_after = starting_time + self.max_api_waiting_time;
        let check_duration = self.max_api_waiting_time / 10;

        let time_to_wait_for_matching_builder = starting_time + self.max_api_waiting_time / 2;

        let mut sent = false;
        let key = (*for_parent, view_num);
        while !sent && Instant::now() < time_to_wait_for_matching_builder {
            // try to broadcast the request to the correct builder state
            if let Some(builder) = self
                .global_state
                .read_arc()
                .await
                .spawned_builder_states
                .get(&key)
            {
                tracing::info!(
                    "Got matching BlockBuilder for {:?}@{view_number}, sending get_available_blocks request",
                    req_msg.requested_vid_commitment
                );
                if let Err(e) = builder
                    .broadcast(MessageType::RequestMessage(req_msg.clone()))
                    .await
                {
                    tracing::warn!(
                        "Error {e} sending get_available_blocks request for parent {:?}@{view_number}",
                        req_msg.requested_vid_commitment
                    );
                }
                sent = true;
            } else {
                tracing::info!(
                    "Failed to get matching BlockBuilder for {:?}@{view_number}, will try again",
                    req_msg.requested_vid_commitment
                );
                async_sleep(check_duration).await
            }
        }

        if !sent {
            // broadcast the request to the best fallback builder state
            if let Err(e) = self
                .global_state
                .read_arc()
                .await
                .get_channel_for_matching_builder_or_highest_view_buider(&(*for_parent, view_num))?
                .broadcast(MessageType::RequestMessage(req_msg.clone()))
                .await
            {
                tracing::warn!(
                    "Error {e} sending get_available_blocks request for parent {:?}@{view_number}",
                    req_msg.requested_vid_commitment
                );
            }
        }

        tracing::debug!(
            "Waiting for response for get_available_blocks with parent {:?}@{view_number}",
            req_msg.requested_vid_commitment
        );

        let response_received = loop {
            match async_timeout(check_duration, response_receiver.recv()).await {
                Err(toe) => {
                    if Instant::now() >= timeout_after {
                        tracing::warn!(%toe, "Couldn't get available blocks in time for parent {:?}@{view_number}",  req_msg.requested_vid_commitment);
                        // lookup into the builder_state_to_last_built_block, if it contains the result, return that otherwise return error
                        if let Some(last_built_block) = self
                            .global_state
                            .read_arc()
                            .await
                            .builder_state_to_last_built_block
                            .get(&(*for_parent, view_num))
                        {
                            tracing::info!(
                                "Returning last built block for parent {:?}@{view_number}",
                                req_msg.requested_vid_commitment
                            );
                            break Ok(last_built_block.clone());
                        }
                        break Err(BuildError::Error {
                            message: "No blocks available".to_string(),
                        });
                    }
                    continue;
                }
                Ok(recv_attempt) => {
                    if let Err(ref e) = recv_attempt {
                        tracing::error!(%e, "Channel closed while getting available blocks for parent {:?}@{view_number}", req_msg.requested_vid_commitment);
                    }
                    break recv_attempt.map_err(|_| BuildError::Error {
                        message: "channel unexpectedly closed".to_string(),
                    });
                }
            }
        };
        // };

        match response_received {
            Ok(response) => {
                let (pub_key, sign_key) = self.builder_keys.clone();
                // sign over the block info
                let signature_over_block_info =
                    <TYPES as NodeType>::BuilderSignatureKey::sign_block_info(
                        &sign_key,
                        response.block_size,
                        response.offered_fee,
                        &response.builder_hash,
                    )
                    .map_err(|e| BuildError::Error {
                        message: format!("Signing over block info failed: {:?}", e),
                    })?;

                // insert the block info into local hashmap
                let initial_block_info = AvailableBlockInfo::<TYPES> {
                    block_hash: response.builder_hash.clone(),
                    block_size: response.block_size,
                    offered_fee: response.offered_fee,
                    signature: signature_over_block_info,
                    sender: pub_key.clone(),
                    _phantom: Default::default(),
                };
                tracing::info!(
                    "Sending available Block info response for (parent {:?}, view_num: {:?}) with block hash: {:?}",
                    req_msg.requested_vid_commitment,
                    view_number,
                    response.builder_hash
                );
                Ok(vec![initial_block_info])
            }

            // We failed to get available blocks
            Err(e) => {
                tracing::warn!(
                    "Failed to get available blocks for parent {:?}@{view_number}",
                    req_msg.requested_vid_commitment
                );
                Err(e)
            }
        }
    }

    async fn claim_block(
        &self,
        block_hash: &BuilderCommitment,
        view_number: u64,
        sender: TYPES::SignatureKey,
        signature: &<<TYPES as NodeType>::SignatureKey as SignatureKey>::PureAssembledSignatureType,
    ) -> Result<AvailableBlockData<TYPES>, BuildError> {
        tracing::info!(
            "Received request for claiming block for (block_hash {:?}, view_num: {:?})",
            block_hash,
            view_number
        );
        // verify the signature
        if !sender.validate(signature, block_hash.as_ref()) {
            tracing::error!("Signature validation failed in claim block");
            return Err(BuildError::Error {
                message: "Signature validation failed in claim block".to_string(),
            });
        }
        let (pub_key, sign_key) = self.builder_keys.clone();
        let view_num = <<TYPES as NodeType>::Time as ConsensusTime>::new(view_number);

        if let Some(block_info) = self
            .global_state
            .read_arc()
            .await
            .block_hash_to_block
            .get(&(block_hash.clone(), view_num))
        {
            tracing::info!(
                "Trying sending vid trigger info for {:?}@{:?}",
                block_hash,
                view_num
            );

            // sign over the builder commitment, as the proposer can computer it based on provide block_payload
            // and the metata data
            let response_block_hash = block_info
                .block_payload
                .builder_commitment(&block_info.metadata);
            let signature_over_builder_commitment =
                <TYPES as NodeType>::BuilderSignatureKey::sign_builder_message(
                    &sign_key,
                    response_block_hash.as_ref(),
                )
                .map_err(|e| BuildError::Error {
                    message: format!("Signing over builder commitment failed: {:?}", e),
                })?;

            let signature_over_fee =
                <TYPES as NodeType>::BuilderSignatureKey::sign_sequencing_fee_marketplace(
                    &sign_key,
                    block_info.offered_fee,
                )
                .map_err(|e| BuildError::Error {
                    message: format!("Signing over sequencing fee failed: {:?}", e),
                })?;

            let block_data = AvailableBlockData::<TYPES> {
                fee: block_info.offered_fee,
                fee_signature: signature_over_fee,
                block_payload: block_info.block_payload.clone(),
                metadata: block_info.metadata.clone(),
                signature: signature_over_builder_commitment,
                sender: pub_key.clone(),
            };
            tracing::info!(
                "Sending Claim Block data for (block_hash {:?}, view_num: {:?})",
                block_hash,
                view_number
            );
            Ok(block_data)
        } else {
            tracing::warn!("Claim Block not found");
            Err(BuildError::Error {
                message: "Block data not found".to_string(),
            })
        }
    }

    async fn builder_address(
        &self,
    ) -> Result<<TYPES as NodeType>::BuilderSignatureKey, BuildError> {
        Ok(self.builder_keys.0.clone())
    }
}
#[async_trait]
impl<TYPES: NodeType> AcceptsTxnSubmits<TYPES> for ProxyGlobalState<TYPES>
where
    TYPES::Transaction: BuilderTransaction,
{
    async fn submit_txns(
        &self,
        txns: Vec<<TYPES as NodeType>::Transaction>,
    ) -> Result<Vec<Commitment<<TYPES as NodeType>::Transaction>>, BuildError> {
        tracing::debug!(
            "Submitting {:?} transactions to the builder states{:?}",
            txns.len(),
            txns.iter().map(|txn| txn.commit()).collect::<Vec<_>>()
        );
        let response = self
            .global_state
            .read_arc()
            .await
            .submit_client_txns(txns)
            .await;

        tracing::debug!(
            "Transaction submitted to the builder states, sending response: {:?}",
            response
        );

        response
    }
}
#[async_trait]
impl<TYPES: NodeType> ReadState for ProxyGlobalState<TYPES>
where
    TYPES::Transaction: BuilderTransaction,
{
    type State = ProxyGlobalState<TYPES>;

    async fn read<T>(
        &self,
        op: impl Send + for<'a> FnOnce(&'a Self::State) -> BoxFuture<'a, T> + 'async_trait,
    ) -> T {
        op(self).await
    }
}

async fn connect_to_events_service<TYPES: NodeType>(
    hotshot_events_api_url: Url,
) -> Option<(
    surf_disco::socket::Connection<
        Event<TYPES>,
        surf_disco::socket::Unsupported,
        EventStreamError,
        TYPES::Base,
    >,
    GeneralStaticCommittee<TYPES, <TYPES as NodeType>::SignatureKey>,
)>
where
    TYPES::Transaction: BuilderTransaction,
{
    let client = surf_disco::Client::<hotshot_events_service::events::Error, TYPES::Base>::new(
        hotshot_events_api_url.clone(),
    );

    if !(client.connect(None).await) {
        return None;
    }

    tracing::info!("Builder client connected to the hotshot events api");

    // client subscrive to hotshot events
    let subscribed_events = client
        .socket("hotshot-events/events")
        .subscribe::<Event<TYPES>>()
        .await
        .ok()?;

    // handle the startup event at the start
    let membership = if let Ok(response) = client
        .get::<StartupInfo<TYPES>>("hotshot-events/startup_info")
        .send()
        .await
    {
        let StartupInfo {
            known_node_with_stake,
            non_staked_node_count,
        } = response;
        let membership: GeneralStaticCommittee<TYPES, <TYPES as NodeType>::SignatureKey> =
            GeneralStaticCommittee::<TYPES, <TYPES as NodeType>::SignatureKey>::create_election(
                known_node_with_stake.clone(),
                known_node_with_stake.clone(),
                0,
            );

        tracing::info!(
            "Startup info: Known nodes with stake: {:?}, Non-staked node count: {:?}",
            known_node_with_stake,
            non_staked_node_count
        );
        Some(membership)
    } else {
        None
    };
    membership.map(|membership| (subscribed_events, membership))
}

async fn connect_to_solver_service<TYPES: NodeType>(
    solver_api_url: Url,
) -> Option<Client<hotshot_events_service::events::Error, TYPES::Base>>
where
    TYPES::Transaction: BuilderTransaction,
{
    let client = surf_disco::Client::<hotshot_events_service::events::Error, TYPES::Base>::new(
        solver_api_url.clone(),
    );

    if !(client.connect(None).await) {
        return None;
    }

    tracing::info!("Builder client connected to the solver api");

    Some(client)
}

/*
Running Non-Permissioned Builder Service
*/
pub async fn run_non_permissioned_standalone_builder_service<TYPES: NodeType<Time = ViewNumber>>(
    // id of namespace to build for
    namespace_id: Option<<TYPES::Transaction as BuilderTransaction>::NamespaceId>,

    // sending a DA proposal from the hotshot to the builder states
    da_sender: BroadcastSender<MessageType<TYPES>>,

    // sending a QC proposal from the hotshot to the builder states
    qc_sender: BroadcastSender<MessageType<TYPES>>,

    // sending a Decide event from the hotshot to the builder states
    decide_sender: BroadcastSender<MessageType<TYPES>>,

    // shared accumulated transactions handle
    tx_sender: BroadcastSender<Arc<ReceivedTransaction<TYPES>>>,

    // Url to (re)connect to for the events stream
    hotshot_events_api_url: Url,

    // URL to submit the bid.
    solver_api_url: Url,

    // Base URL to get the bid.
    //
    // Forms a bid ULR with appended view number and "bundle".
    bid_base_url: Url,

    // Bid configuration.
    bid_config: BidConfig,
) -> Result<(), anyhow::Error>
where
    TYPES::Transaction: BuilderTransaction,
{
    // connection to the events stream
    let connected = connect_to_events_service(hotshot_events_api_url.clone()).await;
    if connected.is_none() {
        return Err(anyhow!(
            "failed to connect to API at {hotshot_events_api_url}"
        ));
    }
    let (mut subscribed_events, mut membership) =
        connected.context("Failed to connect to events service")?;

    loop {
        let event = subscribed_events.next().await;
        //tracing::debug!("Builder Event received from HotShot: {:?}", event);
        match event {
            Some(Ok(event)) => {
                match event.event {
                    EventType::Error { error } => {
                        tracing::error!("Error event in HotShot: {:?}", error);
                    }
                    // tx event
                    EventType::Transactions { transactions } => {
                        if let Err(e) = handle_received_txns(
                            &tx_sender,
                            transactions,
                            TransactionSource::HotShot,
                            namespace_id,
                        )
                        .await
                        {
                            tracing::warn!("Failed to handle transactions; {:?}", e);
                        }
                    }
                    // decide event
                    EventType::Decide {
                        block_size: _,
                        leaf_chain,
                        qc: _,
                    } => {
                        let latest_decide_view_num = leaf_chain[0].leaf.view_number();
                        handle_decide_event(&decide_sender, latest_decide_view_num).await;
                    }
                    // DA proposal event
                    EventType::DaProposal { proposal, sender } => {
                        // get the leader for current view
                        let leader = membership.leader(proposal.data.view_number);
                        // get the committee mstatked node count
                        let total_nodes = membership.total_nodes();

                        handle_da_event(
                            &da_sender,
                            proposal,
                            sender,
                            leader,
                            NonZeroUsize::new(total_nodes).unwrap_or(NonZeroUsize::MIN),
                            namespace_id,
                        )
                        .await;
                    }
                    // QC proposal event
                    EventType::QuorumProposal { proposal, sender } => {
                        // get the leader for current view
                        let leader = membership.leader(proposal.data.view_number);
                        handle_qc_event(&qc_sender, Arc::new(proposal), sender, leader).await;
                    }
                    // View finished event
                    EventType::ViewFinished { view_number } => {
                        handle_view_finished::<TYPES>(
                            view_number,
                            solver_api_url.clone(),
                            bid_base_url.clone(),
                            bid_config.clone(),
                        )
                        .await?
                    }
                    _ => {
                        tracing::error!("Unhandled event from Builder");
                    }
                }
            }
            Some(Err(e)) => {
                tracing::error!("Error in the event stream: {:?}", e);
            }
            None => {
                tracing::error!("Event stream ended");
                let connected = connect_to_events_service(hotshot_events_api_url.clone()).await;
                if connected.is_none() {
                    return Err(anyhow!(
                        "failed to reconnect to API at {hotshot_events_api_url}"
                    ));
                }
                (subscribed_events, membership) =
                    connected.context("Failed to reconnect to events service")?;
            }
        }
    }
}

/*
Running Permissioned Builder Service
*/
pub async fn run_permissioned_standalone_builder_service<
    TYPES: NodeType<Time = ViewNumber>,
    I: NodeImplementation<TYPES>,
>(
    // id of namespace to build for. None if building for all namespaces
    namespace_id: Option<<TYPES::Transaction as BuilderTransaction>::NamespaceId>,

    // sending received transactions
    tx_sender: BroadcastSender<Arc<ReceivedTransaction<TYPES>>>,

    // sending a DA proposal from the hotshot to the builder states
    da_sender: BroadcastSender<MessageType<TYPES>>,

    // sending a QC proposal from the hotshot to the builder states
    qc_sender: BroadcastSender<MessageType<TYPES>>,

    // sending a Decide event from the hotshot to the builder states
    decide_sender: BroadcastSender<MessageType<TYPES>>,

    // hotshot context handle
    hotshot_handle: Arc<SystemContextHandle<TYPES, I>>,

    // URL to submit the bid.
    solver_api_url: Url,

    // Base URL to get the bid.
    //
    // Forms a bid ULR with appended view number and "bundle".
    bid_base_url: Url,

    // Bid configuration.
    bid_config: BidConfig,
) -> Result<(), anyhow::Error>
where
    TYPES::Transaction: BuilderTransaction,
{
    let mut event_stream = hotshot_handle.event_stream();
    loop {
        tracing::debug!("Waiting for events from HotShot");
        match event_stream.next().await {
            None => {
                tracing::error!("Didn't receive any event from the HotShot event stream");
            }
            Some(event) => {
                match event.event {
                    // error event
                    EventType::Error { error } => {
                        tracing::error!("Error event in HotShot: {:?}", error);
                    }
                    // tx event
                    EventType::Transactions { transactions } => {
                        if let Err(e) = handle_received_txns(
                            &tx_sender,
                            transactions,
                            TransactionSource::HotShot,
                            namespace_id,
                        )
                        .await
                        {
                            tracing::warn!("Failed to handle transactions; {:?}", e);
                        }
                    }
                    // decide event
                    EventType::Decide { leaf_chain, .. } => {
                        let latest_decide_view_number = leaf_chain[0].leaf.view_number();

                        handle_decide_event(&decide_sender, latest_decide_view_number).await;
                    }
                    // DA proposal event
                    EventType::DaProposal { proposal, sender } => {
                        // get the leader for current view
                        let leader = hotshot_handle.leader(proposal.data.view_number).await;
                        // get the committee staked node count
                        let total_nodes = hotshot_handle.total_nodes();

                        handle_da_event(
                            &da_sender,
                            proposal,
                            sender,
                            leader,
                            total_nodes,
                            namespace_id,
                        )
                        .await;
                    }
                    // QC proposal event
                    EventType::QuorumProposal { proposal, sender } => {
                        // get the leader for current view
                        let leader = hotshot_handle.leader(proposal.data.view_number).await;
                        handle_qc_event(&qc_sender, Arc::new(proposal), sender, leader).await;
                    }
                    // View finished event
                    EventType::ViewFinished { view_number } => {
                        handle_view_finished::<TYPES>(
                            view_number,
                            solver_api_url.clone(),
                            bid_base_url.clone(),
                            bid_config.clone(),
                        )
                        .await?
                    }
                    _ => {
                        tracing::error!("Unhandled event from Builder: {:?}", event.event);
                    }
                }
            }
        }
    }
}

/*
Utility functions to handle the hotshot events
*/
async fn handle_da_event<TYPES: NodeType>(
    da_channel_sender: &BroadcastSender<MessageType<TYPES>>,
    da_proposal: Proposal<TYPES, DaProposal<TYPES>>,
    sender: <TYPES as NodeType>::SignatureKey,
    leader: <TYPES as NodeType>::SignatureKey,
    total_nodes: NonZeroUsize,
    namespace_id: Option<<TYPES::Transaction as BuilderTransaction>::NamespaceId>,
) where
    TYPES::Transaction: BuilderTransaction,
{
    tracing::debug!(
        "DaProposal: Leader: {:?} for the view: {:?}",
        leader,
        da_proposal.data.view_number
    );

    // get the encoded transactions hash
    let encoded_txns_hash = Sha256::digest(&da_proposal.data.encoded_transactions);
    // check if the sender is the leader and the signature is valid; if yes, broadcast the DA proposal
    if leader == sender && sender.validate(&da_proposal.signature, &encoded_txns_hash) {
        let view_number = da_proposal.data.view_number;
        tracing::debug!(
            "Sending DA proposal to the builder states for view {:?}",
            view_number
        );

        // form a block payload from the encoded transactions
        let block_payload = <TYPES::BlockPayload as BlockPayload<TYPES>>::from_bytes(
            &da_proposal.data.encoded_transactions,
            &da_proposal.data.metadata,
        );
        // get the builder commitment from the block payload
        let builder_commitment = block_payload.builder_commitment(&da_proposal.data.metadata);

        let txn_commitments = match namespace_id {
            Some(namespace_id) => {
                // we don't need to keep transactions from other namespaces
                block_payload
                    .transactions(&da_proposal.data.metadata)
                    .filter(|txn| txn.namespace_id() != namespace_id)
                    .map(|txn| txn.commit())
                    .collect()
            }
            None => block_payload
                .transactions(&da_proposal.data.metadata)
                .map(|txn| txn.commit())
                .collect(),
        };

        let da_msg = DaProposalMessage {
            view_number,
            txn_commitments,
            num_nodes: total_nodes.into(),
            sender,
            builder_commitment,
        };

        if let Err(e) = da_channel_sender
            .broadcast(MessageType::DaProposalMessage(Arc::new(da_msg)))
            .await
        {
            tracing::warn!(
                "Error {e}, failed to send DA proposal to builder states for view {:?}",
                view_number
            );
        }
    } else {
        tracing::error!("Validation Failure on DaProposal for view {:?}: Leader for the current view: {:?} and sender: {:?}", da_proposal.data.view_number, leader, sender);
    }
}

async fn handle_qc_event<TYPES: NodeType>(
    qc_channel_sender: &BroadcastSender<MessageType<TYPES>>,
    qc_proposal: Arc<Proposal<TYPES, QuorumProposal<TYPES>>>,
    sender: <TYPES as NodeType>::SignatureKey,
    leader: <TYPES as NodeType>::SignatureKey,
) where
    TYPES::Transaction: BuilderTransaction,
{
    tracing::debug!(
        "QCProposal: Leader: {:?} for the view: {:?}",
        leader,
        qc_proposal.data.view_number
    );

    let leaf = Leaf::from_quorum_proposal(&qc_proposal.data);

    // check if the sender is the leader and the signature is valid; if yes, broadcast the QC proposal
    if sender == leader && sender.validate(&qc_proposal.signature, leaf.commit().as_ref()) {
        let qc_msg = QCMessage::<TYPES> {
            proposal: qc_proposal,
            sender: leader,
        };
        let view_number = qc_msg.proposal.data.view_number;
        tracing::debug!(
            "Sending QC proposal to the builder states for view {:?}",
            view_number
        );
        if let Err(e) = qc_channel_sender
            .broadcast(MessageType::QCMessage(qc_msg))
            .await
        {
            tracing::warn!(
                "Error {e}, failed to send QC proposal to builder states for view {:?}",
                view_number
            );
        }
    } else {
        tracing::error!("Validation Failure on QCProposal for view {:?}: Leader for the current view: {:?} and sender: {:?}", qc_proposal.data.view_number, leader, sender);
    }
}

async fn handle_decide_event<TYPES: NodeType>(
    decide_channel_sender: &BroadcastSender<MessageType<TYPES>>,
    latest_decide_view_number: TYPES::Time,
) where
    TYPES::Transaction: BuilderTransaction,
{
    let decide_msg: DecideMessage<TYPES> = DecideMessage::<TYPES> {
        latest_decide_view_number,
    };
    tracing::debug!(
        "Sending Decide event to builder states for view {:?}",
        latest_decide_view_number
    );
    if let Err(e) = decide_channel_sender
        .broadcast(MessageType::DecideMessage(decide_msg))
        .await
    {
        tracing::warn!(
            "Error {e}, failed to send Decide event to builder states for view {:?}",
            latest_decide_view_number
        );
    }
}

pub(crate) async fn handle_received_txns<TYPES: NodeType>(
    tx_sender: &BroadcastSender<Arc<ReceivedTransaction<TYPES>>>,
    mut txns: Vec<TYPES::Transaction>,
    source: TransactionSource,
    namespace_id: Option<<TYPES::Transaction as BuilderTransaction>::NamespaceId>,
) -> Result<Vec<Commitment<<TYPES as NodeType>::Transaction>>, BuildError>
where
    TYPES::Transaction: BuilderTransaction,
{
    if let Some(namespace_id) = namespace_id {
        txns.retain(|txn| txn.namespace_id() == namespace_id);
    }
    let mut results = Vec::with_capacity(txns.len());
    let time_in = Instant::now();
    for tx in txns.into_iter() {
        let commit = tx.commit();
        results.push(commit);
        let res = tx_sender
            .broadcast(Arc::new(ReceivedTransaction {
                tx,
                source: source.clone(),
                commit,
                time_in,
            }))
            .await;
        if res.is_err() {
            tracing::warn!("failed to broadcast txn with commit {:?}", commit);
        }
    }
    Ok(results)
}

pub(crate) async fn handle_view_finished<TYPES: NodeType<Time = ViewNumber>>(
    view_number: TYPES::Time,
    solver_api_url: Url,
    bid_base_url: Url,
    bid_config: BidConfig,
) -> Result<(), BuildError>
where
    TYPES::Transaction: BuilderTransaction,
{
    // We submit a bid three views in advance.
    let bid_tx = from_bid_config(bid_config, view_number + 3, bid_base_url)?;

    let solver_client = match connect_to_solver_service::<TYPES>(solver_api_url).await {
        Some(client) => client,
        None => {
            return Err(BuildError::Error {
                message: format!("Failed to connect to the solver service."),
            });
        }
    };

    if let Err(e) = solver_client
        .post::<()>("submit_bid")
        .body_json(&bid_tx)
        .unwrap()
        .send()
        .await
    {
        return Err(BuildError::Error {
            message: format!("Failed to submit the bid: {}", e),
        });
    }

    Ok(())
}
