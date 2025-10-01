use crate::test_utils::TestRandom;
use crate::*;
use derivative::Derivative;
use serde::de::{Deserializer, Error as _};
use serde::{Deserialize, Serialize};
use ssz::{Decode, Encode};
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
pub struct SignedExecutionPayloadEnvelope<E: EthSpec> {
    #[superstruct(only(Gloas), partial_getter(rename = "message_gloas"))]
    pub message: ExecutionPayloadEnvelopeGloas<E>,
    #[superstruct(only(NextFork), partial_getter(rename = "message_next_fork"))]
    pub message: crate::execution_payload_envelope::ExecutionPayloadEnvelopeNextFork<E>,
    pub signature: Signature,
}

impl<E: EthSpec> SignedExecutionPayloadEnvelope<E> {
    /// Create a new `SignedExecutionPayloadEnvelope` from an `ExecutionPayloadEnvelope` and `Signature`.
    pub fn from_envelope(envelope: ExecutionPayloadEnvelope<E>, signature: Signature) -> Self {
        match envelope {
            ExecutionPayloadEnvelope::Gloas(message) => SignedExecutionPayloadEnvelope::Gloas(
                signed_execution_payload_envelope::SignedExecutionPayloadEnvelopeGloas {
                    message,
                    signature,
                },
            ),
            ExecutionPayloadEnvelope::NextFork(message) => {
                SignedExecutionPayloadEnvelope::NextFork(
                    signed_execution_payload_envelope::SignedExecutionPayloadEnvelopeNextFork {
                        message,
                        signature,
                    },
                )
            }
        }
    }

    pub fn message(&self) -> ExecutionPayloadEnvelopeRef<'_, E> {
        match self {
            Self::Gloas(signed) => ExecutionPayloadEnvelopeRef::Gloas(&signed.message),
            Self::NextFork(signed) => ExecutionPayloadEnvelopeRef::NextFork(&signed.message),
        }
    }

    /// Custom SSZ decoder that takes a `ForkName` as context.
    pub fn from_ssz_bytes_for_fork(
        bytes: &[u8],
        fork_name: ForkName,
    ) -> Result<Self, ssz::DecodeError> {
        match fork_name {
            ForkName::Gloas => SignedExecutionPayloadEnvelopeGloas::from_ssz_bytes(bytes)
                .map(SignedExecutionPayloadEnvelope::Gloas),
            _ => Err(ssz::DecodeError::BytesInvalid(format!(
                "SignedExecutionPayloadEnvelope does not support fork {fork_name:?}"
            ))),
        }
    }

    /// Returns the maximum theoretical size of a SignedExecutionPayloadEnvelope.
    /// This calculates the size by taking a full signed envelope with default payload,
    /// then adding the maximum execution payload size, which has a max size ~16 GiB for future proofing.
    pub fn max_size(spec: &ChainSpec) -> usize {
        // Create a full envelope with maximum-sized variable fields but default payload
        let full_envelope = ExecutionPayloadEnvelope::<E>::full(spec);

        let signed_envelope = Self::from_envelope(full_envelope, Signature::empty());

        // Get size of signed envelope with default payload
        let signed_envelope_with_default_payload_size = signed_envelope.as_ssz_bytes().len();

        let default_payload_size = signed_envelope
            .message()
            .payload()
            .clone_from_ref()
            .as_ssz_bytes()
            .len();

        // Calculate max size: signed envelope - default payload + max payload
        signed_envelope_with_default_payload_size - default_payload_size
            + ExecutionPayload::<E>::max_execution_payload_bellatrix_size()
    }
}

impl<'de, E: EthSpec> ContextDeserialize<'de, ForkName> for SignedExecutionPayloadEnvelope<E> {
    fn context_deserialize<D>(deserializer: D, context: ForkName) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value: Self = Deserialize::deserialize(deserializer)?;

        match (context, &value) {
            (ForkName::Gloas, Self::Gloas { .. }) => Ok(value),
            _ => Err(D::Error::custom(format!(
                "SignedExecutionPayloadEnvelope does not support fork {context:?}"
            ))),
        }
    }
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
