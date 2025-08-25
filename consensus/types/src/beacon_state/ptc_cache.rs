#![allow(clippy::arithmetic_side_effects)]

use crate::*;
use core::num::NonZeroUsize;
use derivative::Derivative;
use serde::{Deserialize, Serialize};
use ssz::{four_byte_option_impl, Decode, DecodeError, Encode};
use ssz_derive::{Decode, Encode};

mod tests;

// Define "legacy" implementations of `Option<Epoch>`, `Option<NonZeroUsize>` which use four bytes
// for encoding the union selector.
four_byte_option_impl!(four_byte_option_epoch, Epoch);
four_byte_option_impl!(four_byte_option_non_zero_usize, NonZeroUsize);

/// Computes and stores the PTC (Payload Timeliness Committee) assignments for an epoch.
/// Provides getters to allow callers to read the PTC for any given slot.
///
/// Similar to CommitteeCache but for PTC assignments using balance-weighted selection.
#[derive(Derivative, Debug, Default, Clone, Serialize, Deserialize, Encode, Decode)]
#[derivative(PartialEq)]
pub struct PTCCache {
    #[ssz(with = "four_byte_option_epoch")]
    initialized_epoch: Option<Epoch>,
    /// Flat list of PTC members for all slots in the epoch
    /// Layout: [slot0_members..., slot1_members..., slot2_members...]
    ptc_shuffling: Vec<usize>,
    /// Position in PTC shuffling for each validator (if any)
    #[derivative(PartialEq(compare_with = "compare_ptc_positions"))]
    ptc_positions: Vec<NonZeroUsizeOption>,
    /// PTC size (constant 512 for Gloas+)
    ptc_size: usize,
    /// Slots per epoch
    slots_per_epoch: u64,
}

/// Equivalence function for `ptc_positions` that ignores trailing `None` entries.
///
/// It can happen that states from different epochs computing the same cache have different
/// numbers of validators in `state.validators()` due to recent deposits. These new validators
/// cannot be active however and will always be omitted from the PTC shuffling. This function checks
/// that two lists of PTC positions are equivalent by ensuring that they are identical on all
/// common entries, and that new entries at the end are all `None`.
///
/// In practice this is only used in tests.
#[allow(clippy::indexing_slicing)]
fn compare_ptc_positions(xs: &Vec<NonZeroUsizeOption>, ys: &Vec<NonZeroUsizeOption>) -> bool {
    use std::cmp::Ordering;

    let (shorter, longer) = match xs.len().cmp(&ys.len()) {
        Ordering::Equal => {
            return xs == ys;
        }
        Ordering::Less => (xs, ys),
        Ordering::Greater => (ys, xs),
    };
    shorter == &longer[..shorter.len()]
        && longer[shorter.len()..]
            .iter()
            .all(|new| *new == NonZeroUsizeOption(None))
}

impl PTCCache {
    /// Create a new `PTCCache` for the given `BeaconState` and `relative_epoch`.
    ///
    /// This cache is expensive to build and should be stored somewhere persistent.
    pub fn initialized<E: EthSpec>(
        state: &BeaconState<E>,
        relative_epoch: RelativeEpoch,
        spec: &ChainSpec,
    ) -> Result<Self, Error> {
        let epoch = relative_epoch.into_epoch(state.current_epoch());

        // Return empty cache for pre-Gloas forks
        if !state.fork_name_unchecked().gloas_enabled() {
            return Ok(Self::default());
        }

        // Get active validators for this epoch (use cached version for performance)
        let active_validator_indices = state.get_cached_active_validator_indices(relative_epoch)?;
        let active_validator_count = active_validator_indices.len();

        // Check we have enough validators for a PTC
        let ptc_size = E::ptc_size();
        if active_validator_count < ptc_size {
            return Err(Error::InsufficientValidators);
        }

        let slots_per_epoch = E::slots_per_epoch();
        let total_ptc_members = ptc_size * slots_per_epoch as usize;

        // Pre-allocate the flat shuffling vector
        let mut ptc_shuffling = Vec::with_capacity(total_ptc_members);

        // Build PTC for each slot
        for slot_offset in 0..slots_per_epoch {
            let slot = epoch.start_slot(slots_per_epoch) + slot_offset;

            // Get seed for this slot's PTC selection (following spec)
            let seed = state.get_ptc_attester_seed(slot, spec)?;

            // Get all beacon committees for this slot and concatenate them (following spec)
            let mut slot_committee_indices = Vec::new();
            let committees_per_slot = state.get_committee_count_at_slot(slot)?;

            for committee_index in 0..committees_per_slot {
                let committee =
                    state.get_beacon_committee(slot, committee_index as CommitteeIndex)?;
                slot_committee_indices.extend(committee.committee.iter());
            }

            // Apply balance-weighted selection to concatenated committees (following spec)
            let slot_ptc = state.compute_balance_weighted_selection(
                &slot_committee_indices,
                &seed,
                ptc_size,
                false, // shuffle_indices=False per spec
                spec,
            )?;

            ptc_shuffling.extend(slot_ptc);
        }

        // Build position lookup (similar to shuffling_positions)
        let mut ptc_positions = vec![NonZeroUsize::new(0).into(); state.validators().len()];
        for (i, &validator_index) in ptc_shuffling.iter().enumerate() {
            if let Some(validator_ptc_positions) = ptc_positions.get_mut(validator_index) {
                *validator_ptc_positions = NonZeroUsize::new(i + 1).into();
            }
        }

        Ok(Self {
            initialized_epoch: Some(epoch),
            ptc_shuffling,
            ptc_positions,
            ptc_size,
            slots_per_epoch,
        })
    }

    /// Returns the PTC for the given `slot`.
    ///
    /// The `slot` must be in the same epoch as `self.initialized_epoch`, otherwise an error is returned.
    pub fn get_ptc<E: EthSpec>(&self, slot: Slot) -> Result<PTC<E>, Error> {
        let epoch = self
            .initialized_epoch
            .ok_or(Error::PTCCacheUninitialized(None))?;
        let slot_offset = epoch
            .position(slot, self.slots_per_epoch)
            .ok_or(Error::SlotOutOfBounds)?;

        let start_index = slot_offset * self.ptc_size;
        let end_index = start_index + self.ptc_size;

        let ptc_indices = self
            .ptc_shuffling
            .get(start_index..end_index)
            .ok_or(Error::SlotOutOfBounds)?;

        Ok(PTC(FixedVector::from(ptc_indices.to_vec())))
    }

    /// Returns the epoch for which this cache was initialized.
    pub fn initialized_epoch(&self) -> Option<Epoch> {
        self.initialized_epoch
    }

    /// Returns `true` if this cache has been initialized at the given `epoch`.
    pub fn is_initialized_at(&self, epoch: Epoch) -> bool {
        Some(epoch) == self.initialized_epoch
    }

    /// Returns the slot during the requested epoch in which the validator is a PTC member.
    /// Returns None if the validator has no PTC assignment in this epoch.
    ///
    /// Implements the spec's get_ptc_assignment(state, epoch, validator_index) function.
    /// Uses linear search through slots to match spec behavior exactly.
    pub fn get_ptc_assignment(&self, validator_index: usize) -> Option<Slot> {
        let epoch = self.initialized_epoch?;

        // Check each slot in the epoch (following spec exactly)
        for slot_offset in 0..self.slots_per_epoch {
            let start_index = (slot_offset as usize) * self.ptc_size;
            let end_index = start_index + self.ptc_size;

            // Check if validator_index is in this slot's PTC
            if let Some(slot_ptc) = self.ptc_shuffling.get(start_index..end_index) {
                if slot_ptc.contains(&validator_index) {
                    let slot = epoch.start_slot(self.slots_per_epoch) + slot_offset;
                    return Some(slot);
                }
            }
        }

        None
    }

    /// Optimized version of get_ptc_assignment using O(1) position lookup.
    /// Returns the first slot where the validator appears in PTC.
    ///
    /// Note: This assumes ptc_positions stores the FIRST occurrence of each validator.
    /// Current implementation may have a bug where it stores LAST occurrence instead.
    pub fn get_ptc_assignment_optimized(&self, validator_index: usize) -> Option<Slot> {
        let epoch = self.initialized_epoch?;

        // O(1) lookup: is validator in PTC at all this epoch?
        let global_position = self
            .ptc_positions
            .get(validator_index)?
            .0
            .map(|pos| pos.get() - 1)?;

        // O(1) math: which slot does this position belong to?
        let slot_offset = global_position / self.ptc_size;
        let slot = epoch.start_slot(self.slots_per_epoch) + (slot_offset as u64);
        Some(slot)
    }
}

#[cfg(feature = "arbitrary")]
impl arbitrary::Arbitrary<'_> for PTCCache {
    fn arbitrary(_u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(Self::default())
    }
}

/// This is a shim struct to ensure that we can encode a `Vec<Option<NonZeroUsize>>` an SSZ union
/// with a four-byte selector. The SSZ specification changed from four bytes to one byte during 2021
/// and we use this shim to avoid breaking the Lighthouse database.
#[derive(Debug, Default, PartialEq, Clone, Serialize, Deserialize)]
#[serde(transparent)]
struct NonZeroUsizeOption(Option<NonZeroUsize>);

impl From<Option<NonZeroUsize>> for NonZeroUsizeOption {
    fn from(opt: Option<NonZeroUsize>) -> Self {
        Self(opt)
    }
}

impl Encode for NonZeroUsizeOption {
    fn is_ssz_fixed_len() -> bool {
        four_byte_option_non_zero_usize::encode::is_ssz_fixed_len()
    }

    fn ssz_fixed_len() -> usize {
        four_byte_option_non_zero_usize::encode::ssz_fixed_len()
    }

    fn ssz_bytes_len(&self) -> usize {
        four_byte_option_non_zero_usize::encode::ssz_bytes_len(&self.0)
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        four_byte_option_non_zero_usize::encode::ssz_append(&self.0, buf)
    }

    fn as_ssz_bytes(&self) -> Vec<u8> {
        four_byte_option_non_zero_usize::encode::as_ssz_bytes(&self.0)
    }
}

impl Decode for NonZeroUsizeOption {
    fn is_ssz_fixed_len() -> bool {
        four_byte_option_non_zero_usize::decode::is_ssz_fixed_len()
    }

    fn ssz_fixed_len() -> usize {
        four_byte_option_non_zero_usize::decode::ssz_fixed_len()
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        four_byte_option_non_zero_usize::decode::from_ssz_bytes(bytes).map(Self)
    }
}
