use crate::test_utils::TestRandom;
use crate::*;
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use test_random_derive::TestRandom;
use tree_hash_derive::TreeHash;

#[derive(TestRandom, TreeHash, Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[context_deserialize(ForkName)]
pub struct PayloadAttestationMessage {
    #[serde(with = "serde_utils::quoted_u64")]
    pub validator_index: u64,
    pub data: PayloadAttestationData,
    pub signature: AggregateSignature,
}

impl PayloadAttestationMessage {
    /// Create a `PayloadAttestationMessage` with an empty signature for signing purposes.
    pub fn empty_for_signing(validator_index: u64, data: PayloadAttestationData) -> Self {
        Self {
            validator_index,
            data,
            signature: AggregateSignature::empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    ssz_and_tree_hash_tests!(PayloadAttestationMessage);
}
