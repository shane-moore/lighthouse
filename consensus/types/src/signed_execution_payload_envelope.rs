use crate::test_utils::TestRandom;
use crate::*;
use educe::Educe;
use serde::de::{Deserializer, Error as _};
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
            Educe
        ),
        cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary)),
        educe(PartialEq, Hash(bound(E: EthSpec))),
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
#[derive(Debug, Clone, Serialize, Encode, Deserialize, TreeHash, Educe)]
#[educe(PartialEq, Hash(bound(E: EthSpec)))]
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
