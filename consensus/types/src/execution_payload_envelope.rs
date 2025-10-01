use crate::test_utils::TestRandom;
use crate::*;
use beacon_block_body::KzgCommitments;
use derivative::Derivative;
use serde::de::{Deserializer, Error as _};
use serde::{Deserialize, Serialize};
use ssz::{Decode, DecodeError};
use ssz_derive::{Decode, Encode};
use superstruct::superstruct;
use test_random_derive::TestRandom;
use tree_hash_derive::TreeHash;

// in all likelihood, this will be superstructed so might as well start early eh?
#[superstruct(
    variants(Gloas, NextFork),
    variant_attributes(
        derive(
            Default,
            Debug,
            Clone,
            Serialize,
            Deserialize,
            Encode,
            Decode,
            TreeHash,
            TestRandom,
            Derivative
        ),
        cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary)),
        derivative(PartialEq, Hash(bound = "E: EthSpec")),
        serde(bound = "E: EthSpec", deny_unknown_fields),
        cfg_attr(feature = "arbitrary", arbitrary(bound = "E: EthSpec"))
    ),
    ref_attributes(
        derive(Debug, PartialEq, TreeHash),
        tree_hash(enum_behaviour = "transparent")
    ),
    cast_error(ty = "Error", expr = "BeaconStateError::IncorrectStateVariant"),
    partial_getter_error(ty = "Error", expr = "BeaconStateError::IncorrectStateVariant")
)]
#[derive(Debug, Clone, Serialize, Encode, Deserialize, TreeHash, Derivative)]
#[derivative(PartialEq, Hash(bound = "E: EthSpec"))]
#[serde(bound = "E: EthSpec", untagged)]
#[ssz(enum_behaviour = "transparent")]
#[tree_hash(enum_behaviour = "transparent")]
pub struct ExecutionPayloadEnvelope<E: EthSpec> {
    #[superstruct(only(Gloas), partial_getter(rename = "payload_gloas"))]
    pub payload: ExecutionPayloadGloas<E>,
    #[superstruct(only(NextFork), partial_getter(rename = "payload_next_fork"))]
    pub payload: ExecutionPayloadGloas<E>,
    pub execution_requests: ExecutionRequests<E>,
    #[serde(with = "serde_utils::quoted_u64")]
    #[superstruct(getter(copy))]
    pub builder_index: u64,
    #[superstruct(getter(copy))]
    pub beacon_block_root: Hash256,
    #[superstruct(getter(copy))]
    pub slot: Slot,
    pub blob_kzg_commitments: KzgCommitments<E>,
    #[superstruct(getter(copy))]
    pub state_root: Hash256,
}

impl<E: EthSpec> SignedRoot for ExecutionPayloadEnvelope<E> {}
impl<'a, E: EthSpec> SignedRoot for ExecutionPayloadEnvelopeRef<'a, E> {}

impl<'a, E: EthSpec> ExecutionPayloadEnvelopeRef<'a, E> {
    pub fn payload(&self) -> ExecutionPayloadRef<'a, E> {
        match self {
            Self::Gloas(envelope) => ExecutionPayloadRef::Gloas(&envelope.payload),
            Self::NextFork(envelope) => ExecutionPayloadRef::Gloas(&envelope.payload),
        }
    }
}

impl<E: EthSpec> ExecutionPayloadEnvelope<E> {
    /// Custom SSZ decoder that takes a `ForkName` as context.
    pub fn from_ssz_bytes_for_fork(bytes: &[u8], fork_name: ForkName) -> Result<Self, DecodeError> {
        match fork_name {
            ForkName::Gloas => ExecutionPayloadEnvelopeGloas::from_ssz_bytes(bytes)
                .map(ExecutionPayloadEnvelope::Gloas),
            _ => Err(DecodeError::BytesInvalid(format!(
                "ExecutionPayloadEnvelope does not support fork {fork_name:?}"
            ))),
        }
    }
}

impl<'de, E: EthSpec> ContextDeserialize<'de, ForkName> for ExecutionPayloadEnvelope<E> {
    fn context_deserialize<D>(deserializer: D, context: ForkName) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value: Self = serde::Deserialize::deserialize(deserializer)?;

        match (context, &value) {
            (ForkName::Gloas, Self::Gloas { .. }) => Ok(value),
            _ => Err(D::Error::custom(format!(
                "ExecutionPayloadEnvelope does not support fork {context:?}"
            ))),
        }
    }
}

/// Trait for creating empty execution payload envelopes.
pub trait EmptyEnvelope {
    /// Returns an empty envelope.
    fn empty(spec: &ChainSpec) -> Self;
}

/// Trait for creating full-sized execution payload envelopes.
pub trait FullEnvelope {
    /// Returns an envelope with maximum-sized variable fields.
    fn full(spec: &ChainSpec) -> Self;
}

impl<E: EthSpec> EmptyEnvelope for ExecutionPayloadEnvelopeGloas<E> {
    /// Returns an empty Gloas execution payload envelope.
    fn empty(spec: &ChainSpec) -> Self {
        ExecutionPayloadEnvelopeGloas {
            payload: ExecutionPayloadGloas::<E>::default(),
            execution_requests: ExecutionRequests::<E>::default(),
            builder_index: 0,
            beacon_block_root: Hash256::zero(),
            slot: spec
                .gloas_fork_epoch
                .expect("gloas enabled")
                .start_slot(E::slots_per_epoch()),
            blob_kzg_commitments: VariableList::empty(),
            state_root: Hash256::zero(),
        }
    }
}

impl<E: EthSpec> FullEnvelope for ExecutionPayloadEnvelopeGloas<E> {
    /// Returns a Gloas execution payload envelope with maximum-sized variable fields.
    fn full(spec: &ChainSpec) -> Self {
        let deposit_request = DepositRequest {
            pubkey: PublicKeyBytes::empty(),
            withdrawal_credentials: Hash256::zero(),
            amount: 0,
            signature: SignatureBytes::empty(),
            index: 0,
        };

        let withdrawal_request = WithdrawalRequest {
            source_address: Address::zero(),
            validator_pubkey: PublicKeyBytes::empty(),
            amount: 0,
        };

        let consolidation_request = ConsolidationRequest {
            source_address: Address::zero(),
            source_pubkey: PublicKeyBytes::empty(),
            target_pubkey: PublicKeyBytes::empty(),
        };

        let kzg_commitment = KzgCommitment::empty_for_testing();

        // Fill variable lists to maximum capacity
        let mut deposits = VariableList::empty();
        for _ in 0..E::MaxDepositRequestsPerPayload::to_usize() {
            deposits.push(deposit_request.clone()).unwrap();
        }

        let mut withdrawals = VariableList::empty();
        for _ in 0..E::MaxWithdrawalRequestsPerPayload::to_usize() {
            withdrawals.push(withdrawal_request.clone()).unwrap();
        }

        let mut consolidations = VariableList::empty();
        for _ in 0..E::MaxConsolidationRequestsPerPayload::to_usize() {
            consolidations.push(consolidation_request.clone()).unwrap();
        }

        let mut blob_kzg_commitments = VariableList::empty();
        for _ in 0..E::MaxBlobCommitmentsPerBlock::to_usize() {
            blob_kzg_commitments.push(kzg_commitment.clone()).unwrap();
        }

        ExecutionPayloadEnvelopeGloas {
            payload: ExecutionPayloadGloas::<E>::default(), // Keep payload as default - we add max payload size separately
            execution_requests: ExecutionRequests {
                deposits,
                withdrawals,
                consolidations,
            },
            builder_index: 0,
            beacon_block_root: Hash256::zero(),
            slot: spec
                .gloas_fork_epoch
                .expect("gloas enabled")
                .start_slot(E::slots_per_epoch()),
            blob_kzg_commitments,
            state_root: Hash256::zero(),
        }
    }
}

impl<E: EthSpec> ExecutionPayloadEnvelope<E> {
    /// Returns an empty envelope
    pub fn empty(spec: &ChainSpec) -> Self {
        // For now, only Gloas is supported
        ExecutionPayloadEnvelope::Gloas(ExecutionPayloadEnvelopeGloas::empty(spec))
    }

    /// Returns an envelope with maximum-sized variable fields
    pub fn full(spec: &ChainSpec) -> Self {
        // For now, only Gloas is supported
        ExecutionPayloadEnvelope::Gloas(ExecutionPayloadEnvelopeGloas::full(spec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MainnetEthSpec;

    mod gloas {
        use super::*;
        ssz_and_tree_hash_tests!(ExecutionPayloadEnvelopeGloas<MainnetEthSpec>);
    }

    mod next_fork {
        use super::*;
        ssz_and_tree_hash_tests!(ExecutionPayloadEnvelopeNextFork<MainnetEthSpec>);
    }
}
