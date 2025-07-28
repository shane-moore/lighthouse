use crate::test_utils::TestRandom;
use crate::*;
use derivative::Derivative;
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use superstruct::superstruct;
use test_random_derive::TestRandom;
use tree_hash_derive::TreeHash;

#[superstruct(
    variants(Gloas, NextFork),
    variant_attributes(
        derive(
            Debug,
            Clone,
            Serialize,
            Deserialize,
            Encode,
            Decode,
            TreeHash,
            TestRandom,
            Derivative,
            arbitrary::Arbitrary
        ),
        derivative(PartialEq, Hash(bound = "E: EthSpec")),
        serde(bound = "E: EthSpec", deny_unknown_fields),
        arbitrary(bound = "E: EthSpec")
    ),
    ref_attributes(
        derive(Debug, PartialEq, TreeHash),
        tree_hash(enum_behaviour = "transparent")
    ),
    cast_error(ty = "Error", expr = "BeaconStateError::IncorrectStateVariant"),
    partial_getter_error(ty = "Error", expr = "BeaconStateError::IncorrectStateVariant")
)]
#[derive(
    Debug, Clone, Serialize, Encode, Deserialize, TreeHash, Derivative, arbitrary::Arbitrary,
)]
#[derivative(PartialEq, Hash(bound = "E: EthSpec"))]
#[serde(bound = "E: EthSpec", untagged)]
#[arbitrary(bound = "E: EthSpec")]
#[ssz(enum_behaviour = "transparent")]
#[tree_hash(enum_behaviour = "transparent")]
#[context_deserialize(ForkName)]
pub struct SignedExecutionPayloadEnvelope<E: EthSpec> {
    #[superstruct(only(Gloas), partial_getter(rename = "message_gloas"))]
    pub message: ExecutionPayloadEnvelopeGloas<E>,
    #[superstruct(only(NextFork), partial_getter(rename = "message_next_fork"))]
    pub message: crate::execution_payload_envelope::ExecutionPayloadEnvelopeNextFork<E>,
    pub signature: Signature,
}

impl<E: EthSpec> SignedExecutionPayloadEnvelope<E> {
    pub fn message(&self) -> ExecutionPayloadEnvelopeRef<E> {
        match self {
            Self::Gloas(ref signed) => ExecutionPayloadEnvelopeRef::Gloas(&signed.message),
            Self::NextFork(ref signed) => ExecutionPayloadEnvelopeRef::NextFork(&signed.message),
        }
    }

    // todo(eip-7732): implement verify_signature since spec calls for verify_execution_payload_envelope_signature
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MainnetEthSpec;

    mod gloas {
        use super::*;
        ssz_and_tree_hash_tests!(SignedExecutionPayloadEnvelopeGloas<MainnetEthSpec>);
    }

    mod next_fork {
        use super::*;
        ssz_and_tree_hash_tests!(SignedExecutionPayloadEnvelopeNextFork<MainnetEthSpec>);
    }
}
