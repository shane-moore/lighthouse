use crate::*;
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[derive(Debug, PartialEq, Clone, Copy, Serialize, Deserialize)]
pub struct PtcDuty {
    /// The validator's index in the validator registry.
    #[serde(with = "serde_utils::quoted_u64")]
    pub validator_index: u64,
    /// The slot at which this validator should perform PTC duties.
    pub slot: Slot,
    /// The validator's pubkey
    pub pubkey: PublicKeyBytes,
}
