//! The incremental processing steps (e.g., signatures verified but not the state transition) is
//! represented as a sequence of wrapper-types around the block. There is a linear progression of
//! types, starting at a `SignedBeaconBlock` and finishing with a `Fully VerifiedBlock` (see
//! diagram below).
//!
//! ```ignore
//!            START
//!              |
//!              ▼
//! SignedExecutionPayloadEnvelope
//!              |
//!              |---------------
//!              |              |
//!              |              ▼
//!              |    GossipVerifiedEnvelope
//!              |              |
//!              |---------------
//!              |
//!              ▼
//!  SignatureVerifiedEnvelope
//!              |
//!              ▼
//!  ExecutionPendingEnvelope
//!              |
//!            await
//!              |
//!              ▼
//!             END
//!
//! ```

use crate::NotifyExecutionLayer;
use crate::block_verification::{PayloadVerificationHandle, PayloadVerificationOutcome};
use crate::envelope_verification_types::{EnvelopeImportData, MaybeAvailableEnvelope};
use crate::execution_payload::PayloadNotifier;
use crate::{BeaconChain, BeaconChainError, BeaconChainTypes};
use derivative::Derivative;
use safe_arith::ArithError;
use slot_clock::SlotClock;
use state_processing::envelope_processing::{EnvelopeProcessingError, envelope_processing};
use state_processing::per_block_processing::compute_timestamp_at_slot;
use state_processing::{BlockProcessingError, VerifySignatures};
use std::sync::Arc;
use tree_hash::TreeHash;
use types::{
    BeaconState, BeaconStateError, ExecutionBlockHash, Hash256, SignedBeaconBlock,
    SignedExecutionPayloadEnvelope, Slot,
};

// TODO(gloas): don't use this redefinition..
macro_rules! envelope_verify {
    ($condition: expr, $result: expr) => {
        if !$condition {
            return Err($result);
        }
    };
}

#[derive(Debug, Clone)]
pub enum EnvelopeError {
    /// The envelope's block root is unknown.
    BlockRootUnknown {
        block_root: Hash256,
    },
    /// The signature is invalid.
    BadSignature,
    /// Envelope doesn't match latest beacon block header
    LatestBlockHeaderMismatch {
        envelope_root: Hash256,
        block_header_root: Hash256,
    },
    /// The builder index doesn't match the committed bid
    BuilderIndexMismatch {
        committed_bid: u64,
        envelope: u64,
    },
    /// The blob KZG commitments root doesn't match the committed bid
    BlobKzgCommitmentsRootMismatch {
        committed_bid: Hash256,
        envelope: Hash256,
    },
    /// The withdrawals root doesn't match the state's latest withdrawals root
    WithdrawalsRootMismatch {
        state: Hash256,
        envelope: Hash256,
    },
    // The gas limit doesn't match the committed bid
    GasLimitMismatch {
        committed_bid: u64,
        envelope: u64,
    },
    // The block hash doesn't match the committed bid
    BlockHashMismatch {
        committed_bid: ExecutionBlockHash,
        envelope: ExecutionBlockHash,
    },
    // The parent hash doesn't match the previous execution payload
    ParentHashMismatch {
        state: ExecutionBlockHash,
        envelope: ExecutionBlockHash,
    },
    // The previous randao didn't match the payload
    PrevRandaoMismatch {
        state: Hash256,
        envelope: Hash256,
    },
    // The timestamp didn't match the payload
    TimestampMismatch {
        state: u64,
        envelope: u64,
    },
    // Blob committments exceeded the maximum
    BlobLimitExceeded {
        max: usize,
        envelope: usize,
    },
    // Invalid state root
    InvalidStateRoot {
        state: Hash256,
        envelope: Hash256,
    },
    // The slot doesn't match the parent block
    SlotMismatch {
        parent_block: Slot,
        envelope: Slot,
    },
    // Some Beacon Chain Error
    BeaconChainError(Arc<BeaconChainError>),
    // Some Beacon State error
    BeaconStateError(BeaconStateError),
    // Some ArithError
    ArithError(ArithError),
    // Some BlockProcessingError (for electra operations)
    BlockProcessingError(BlockProcessingError),
}

impl From<BeaconChainError> for EnvelopeError {
    fn from(e: BeaconChainError) -> Self {
        EnvelopeError::BeaconChainError(Arc::new(e))
    }
}

impl From<BeaconStateError> for EnvelopeError {
    fn from(e: BeaconStateError) -> Self {
        EnvelopeError::BeaconStateError(e)
    }
}

impl From<ArithError> for EnvelopeError {
    fn from(e: ArithError) -> Self {
        EnvelopeError::ArithError(e)
    }
}

impl From<EnvelopeProcessingError> for EnvelopeError {
    fn from(e: EnvelopeProcessingError) -> Self {
        match e {
            EnvelopeProcessingError::BadSignature => EnvelopeError::BadSignature,
            EnvelopeProcessingError::BeaconStateError(e) => EnvelopeError::BeaconStateError(e),
            EnvelopeProcessingError::BlockProcessingError(e) => {
                EnvelopeError::BlockProcessingError(e)
            }
        }
    }
}

/// A wrapper around a `SignedExecutionPayloadEnvelope` that indicates it has been approved for re-gossiping on
/// the p2p network.
#[derive(Derivative)]
#[derivative(Debug(bound = "T: BeaconChainTypes"))]
pub struct GossipVerifiedEnvelope<T: BeaconChainTypes> {
    pub signed_envelope: Arc<SignedExecutionPayloadEnvelope<T::EthSpec>>,
    pub parent_block: Arc<SignedBeaconBlock<T::EthSpec>>,
    pub pre_state: Box<BeaconState<T::EthSpec>>,
}

impl<T: BeaconChainTypes> GossipVerifiedEnvelope<T> {
    pub fn new(
        signed_envelope: Arc<SignedExecutionPayloadEnvelope<T::EthSpec>>,
        chain: &BeaconChain<T>,
    ) -> Result<Self, EnvelopeError> {
        let envelope = signed_envelope.message();
        let payload = envelope.payload();
        let block_root = envelope.beacon_block_root();

        // TODO(gloas): this check would fail if the block didn't pass validation right?

        // check that we've seen the parent block of this envelope
        let fork_choice_read_lock = chain.canonical_head.fork_choice_read_lock();
        if !fork_choice_read_lock.contains_block(&block_root) {
            return Err(EnvelopeError::BlockRootUnknown { block_root });
        }
        drop(fork_choice_read_lock);

        let parent_block = chain
            .get_full_block(&block_root)?
            .ok_or_else(|| EnvelopeError::from(BeaconChainError::MissingBeaconBlock(block_root)))
            .map(Arc::new)?;
        let execution_bid = &parent_block
            .message()
            .body()
            .signed_execution_bid()?
            .message;

        // TODO(gloas): check we're within the bounds of the slot (probably)
        // I think a timestamp check like this is on the beacon block but need to check.
        if envelope.slot() != parent_block.slot() {
            return Err(EnvelopeError::SlotMismatch {
                parent_block: parent_block.slot(),
                envelope: envelope.slot(),
            });
        }

        // TODO(gloas): check that we haven't seen another valid `SignedExecutionPayloadEnvelope`
        // for this block root from this builder - envelope status table check

        // builder index matches committed bid
        if envelope.builder_index() != execution_bid.builder_index {
            return Err(EnvelopeError::BuilderIndexMismatch {
                committed_bid: execution_bid.builder_index,
                envelope: envelope.builder_index(),
            });
        }

        // the block hash should match the block hash of the execution bid
        if payload.block_hash() != execution_bid.block_hash {
            return Err(EnvelopeError::BlockHashMismatch {
                committed_bid: execution_bid.block_hash,
                envelope: payload.block_hash(),
            });
        }

        // TODO(gloas): expensive load here.. check this
        let parent_state = chain
            // TODO(gloas): may need a get_block_state to get the right state here..
            .get_state(
                &parent_block.message().state_root(),
                Some(parent_block.slot()),
                true,
            )?
            .ok_or_else(|| {
                EnvelopeError::from(BeaconChainError::MissingBeaconState(
                    parent_block.message().state_root(),
                ))
            })?;

        // verify the signature
        if !signed_envelope.verify_signature(&parent_state, &chain.spec)? {
            return Err(EnvelopeError::BadSignature);
        }

        Ok(Self {
            signed_envelope,
            parent_block,
            pre_state: Box::new(parent_state),
        })
    }

    pub fn envelope_cloned(&self) -> Arc<SignedExecutionPayloadEnvelope<T::EthSpec>> {
        self.signed_envelope.clone()
    }
}

pub trait IntoExecutionPendingEnvelope<T: BeaconChainTypes>: Sized {
    fn into_execution_pending_envelope(
        self,
        chain: &Arc<BeaconChain<T>>,
        notify_execution_layer: NotifyExecutionLayer,
    ) -> Result<ExecutionPendingEnvelope<T>, EnvelopeError>;
}

pub struct ExecutionPendingEnvelope<T: BeaconChainTypes> {
    pub signed_envelope: MaybeAvailableEnvelope<T::EthSpec>,
    pub import_data: EnvelopeImportData<T::EthSpec>,
    pub payload_verification_handle: PayloadVerificationHandle,
}

impl<T: BeaconChainTypes> IntoExecutionPendingEnvelope<T> for GossipVerifiedEnvelope<T> {
    fn into_execution_pending_envelope(
        self,
        chain: &Arc<BeaconChain<T>>,
        notify_execution_layer: NotifyExecutionLayer,
    ) -> Result<ExecutionPendingEnvelope<T>, EnvelopeError> {
        let signed_envelope = self.signed_envelope;
        let envelope = signed_envelope.message();
        let payload = &envelope.payload();

        // verify signature already done
        let mut state = *self.pre_state;

        // all state modifications are done in envelope_processing (called at the bottom of this function)
        // so here perform the consistency check with the beacon block on a copy of the latest block header
        // and let it be modified later in envelope_processing
        let previous_state_root = state.canonical_root()?;
        if state.latest_block_header().state_root == Hash256::default() {
            let mut copy_of_latest_block_header = state.latest_block_header().clone();
            copy_of_latest_block_header.state_root = previous_state_root;

            // Verify consistency with the beacon block
            if !envelope.beacon_block_root() == copy_of_latest_block_header.tree_hash_root() {
                return Err(EnvelopeError::LatestBlockHeaderMismatch {
                    envelope_root: envelope.beacon_block_root(),
                    block_header_root: copy_of_latest_block_header.tree_hash_root(),
                });
            };
        }

        // the check about the slots matching is already done in the GossipVerifiedEnvelope

        // Verify consistency with the committed bid
        let committed_bid = state.latest_execution_bid()?;
        // builder index match already verified
        if committed_bid.blob_kzg_commitments_root
            != envelope.blob_kzg_commitments().tree_hash_root()
        {
            return Err(EnvelopeError::BlobKzgCommitmentsRootMismatch {
                committed_bid: committed_bid.blob_kzg_commitments_root,
                envelope: envelope.blob_kzg_commitments().tree_hash_root(),
            });
        };

        // Verify the withdrawals root
        envelope_verify!(
            payload.withdrawals()?.tree_hash_root() == *state.latest_withdrawals_root()?,
            EnvelopeError::WithdrawalsRootMismatch {
                state: *state.latest_withdrawals_root()?,
                envelope: payload.withdrawals()?.tree_hash_root(),
            }
            .into()
        );

        // Verify the gas limit
        envelope_verify!(
            payload.gas_limit() == committed_bid.gas_limit,
            EnvelopeError::GasLimitMismatch {
                committed_bid: committed_bid.gas_limit,
                envelope: payload.gas_limit(),
            }
            .into()
        );
        // Verify the block hash already done in the GossipVerifiedEnvelope

        // Verify consistency of the parent hash with respect to the previous execution payload
        envelope_verify!(
            payload.parent_hash() == *state.latest_block_hash()?,
            EnvelopeError::ParentHashMismatch {
                state: *state.latest_block_hash()?,
                envelope: payload.parent_hash(),
            }
            .into()
        );

        // Verify prev_randao
        envelope_verify!(
            payload.prev_randao() == *state.get_randao_mix(state.current_epoch())?,
            EnvelopeError::PrevRandaoMismatch {
                state: *state.get_randao_mix(state.current_epoch())?,
                envelope: payload.prev_randao(),
            }
            .into()
        );

        // Verify the timestamp
        let state_timestamp = compute_timestamp_at_slot(&state, state.slot(), chain.spec.as_ref())?;
        envelope_verify!(
            payload.timestamp() == state_timestamp,
            EnvelopeError::TimestampMismatch {
                state: state_timestamp,
                envelope: payload.timestamp(),
            }
            .into()
        );

        // Verify the commitments are under limit
        let max_blobs = chain.spec.max_blobs_per_block(state.current_epoch()) as usize;
        envelope_verify!(
            envelope.blob_kzg_commitments().len() <= max_blobs,
            EnvelopeError::BlobLimitExceeded {
                max: max_blobs,
                envelope: envelope.blob_kzg_commitments().len(),
            }
            .into()
        );

        // Verify the execution payload is valid
        let payload_notifier =
            PayloadNotifier::from_envelope(chain.clone(), envelope, notify_execution_layer)?;
        let block_root = envelope.beacon_block_root();
        let slot = self.parent_block.slot();

        let payload_verification_future = async move {
            let chain = payload_notifier.chain.clone();
            // TODO:(gloas): timing
            if let Some(started_execution) = chain.slot_clock.now_duration() {
                chain.block_times_cache.write().set_time_started_execution(
                    block_root,
                    slot,
                    started_execution,
                );
            }

            let payload_verification_status = payload_notifier.notify_new_payload().await?;
            Ok(PayloadVerificationOutcome {
                payload_verification_status,
                // This fork is after the merge so it'll never be the merge transition block
                is_valid_merge_transition_block: false,
            })
        };
        // Spawn the payload verification future as a new task, but don't wait for it to complete.
        // The `payload_verification_future` will be awaited later to ensure verification completed
        // successfully.
        let payload_verification_handle = chain
            .task_executor
            .spawn_handle(
                payload_verification_future,
                "execution_payload_verification",
            )
            .ok_or(BeaconChainError::RuntimeShutdown)?;

        // All the state modifications are done in envelope_processing
        envelope_processing(
            &mut state,
            &signed_envelope,
            VerifySignatures::False,
            &chain.spec,
        )?;

        // TODO(gloas): if verify
        envelope_verify!(
            state.canonical_root()? == envelope.state_root(),
            EnvelopeError::InvalidStateRoot {
                state: state.canonical_root()?,
                envelope: envelope.state_root(),
            }
        );

        Ok(ExecutionPendingEnvelope {
            signed_envelope: MaybeAvailableEnvelope::AvailabilityPending {
                block_hash: payload.block_hash(),
                envelope: signed_envelope,
            },
            import_data: EnvelopeImportData {
                block_root,
                parent_block: self.parent_block,
                post_state: Box::new(state),
            },
            payload_verification_handle,
        })
    }
}

impl<T: BeaconChainTypes> IntoExecutionPendingEnvelope<T>
    for Arc<SignedExecutionPayloadEnvelope<T::EthSpec>>
{
    fn into_execution_pending_envelope(
        self,
        chain: &Arc<BeaconChain<T>>,
        notify_execution_layer: NotifyExecutionLayer,
    ) -> Result<ExecutionPendingEnvelope<T>, EnvelopeError> {
        // TODO(gloas): figure out how this should be refactored..
        GossipVerifiedEnvelope::new(self, chain)?
            .into_execution_pending_envelope(chain, notify_execution_layer)
    }
}
