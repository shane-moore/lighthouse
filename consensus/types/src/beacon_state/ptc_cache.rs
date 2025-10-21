#![allow(clippy::arithmetic_side_effects)]

use crate::*;
use std::collections::HashMap;
use std::sync::Arc;

mod tests;

/// Computes and stores the PTC (Payload Timeliness Committee) assignments for an epoch.
/// Provides getters to allow callers to read the PTC for any given slot.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PTCCache {
    initialized_epoch: Option<Epoch>,
    ptc_shuffling: Vec<usize>,
    ptc_slot_assignments: HashMap<usize, u8>,
    ptc_size: usize,
    slots_per_epoch: u64,
}

impl PTCCache {
    /// Create a new `PTCCache` for the given `BeaconState` and `epoch`.
    ///
    /// This cache is expensive to build and should be stored somewhere persistent.
    pub fn initialized<E: EthSpec>(
        state: &BeaconState<E>,
        epoch: Epoch,
        spec: &ChainSpec,
    ) -> Result<Arc<Self>, Error> {
        // Validate that the epoch is within valid range relative to current state
        let relative_epoch = RelativeEpoch::from_epoch(state.current_epoch(), epoch)?;

        // Check that the required RANDAO seed is available in state
        let reqd_randao_epoch = epoch
            .saturating_sub(spec.min_seed_lookahead)
            .saturating_sub(1u64);

        if reqd_randao_epoch < state.min_randao_epoch() || epoch > state.current_epoch() + 1 {
            return Err(Error::EpochOutOfBounds);
        }

        // May cause divide-by-zero errors
        if E::slots_per_epoch() == 0 {
            return Err(Error::ZeroSlotsPerEpoch);
        }

        // Return empty cache for pre-Gloas forks
        if !state.fork_name_unchecked().gloas_enabled() {
            return Ok(Arc::new(Self::default()));
        }

        let active_validator_indices = state.get_cached_active_validator_indices(relative_epoch)?;
        if active_validator_indices.is_empty() {
            return Err(Error::InsufficientValidators);
        }

        let ptc_size = E::ptc_size();
        let slots_per_epoch = E::slots_per_epoch();
        let total_ptc_members = ptc_size * slots_per_epoch as usize;

        let mut ptc_shuffling = Vec::with_capacity(total_ptc_members);

        // Build PTC for each slot
        for slot_offset in 0..slots_per_epoch {
            let slot = epoch.start_slot(slots_per_epoch) + slot_offset;

            let seed = state.get_ptc_attester_seed(slot, spec)?;

            let mut slot_committee_indices = Vec::new();
            let committees_per_slot = state.get_committee_count_at_slot(slot)?;

            for committee_index in 0..committees_per_slot {
                let committee =
                    state.get_beacon_committee(slot, committee_index as CommitteeIndex)?;
                slot_committee_indices.extend(committee.committee.iter());
            }

            let slot_ptc = state.compute_balance_weighted_selection(
                &slot_committee_indices,
                &seed,
                ptc_size,
                false,
                spec,
            )?;

            ptc_shuffling.extend(slot_ptc);
        }

        // Build slot assignment lookup
        let mut ptc_slot_assignments = HashMap::new();
        for slot_offset in 0..slots_per_epoch {
            let start_index = (slot_offset as usize) * ptc_size;
            let end_index = start_index + ptc_size;

            for &validator_index in &ptc_shuffling[start_index..end_index] {
                ptc_slot_assignments
                    .entry(validator_index)
                    .or_insert(slot_offset as u8);
            }
        }

        Ok(Arc::new(Self {
            initialized_epoch: Some(epoch),
            ptc_shuffling,
            ptc_slot_assignments,
            ptc_size,
            slots_per_epoch,
        }))
    }

    /// Returns the PTC for the given `slot`.
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
    pub fn get_ptc_assignment(&self, validator_index: usize) -> Option<Slot> {
        let epoch = self.initialized_epoch?;

        for slot_offset in 0..self.slots_per_epoch {
            let start_index = (slot_offset as usize) * self.ptc_size;
            let end_index = start_index + self.ptc_size;

            if let Some(slot_ptc) = self.ptc_shuffling.get(start_index..end_index) {
                if slot_ptc.contains(&validator_index) {
                    let slot = epoch.start_slot(self.slots_per_epoch) + slot_offset;
                    return Some(slot);
                }
            }
        }

        None
    }

    /// Optimized version of get_ptc_assignment.
    pub fn get_ptc_assignment_optimized(&self, validator_index: usize) -> Option<Slot> {
        let epoch = self.initialized_epoch?;

        let slot_offset = *self.ptc_slot_assignments.get(&validator_index)?;

        Some(epoch.start_slot(self.slots_per_epoch) + slot_offset as u64)
    }
}

#[cfg(feature = "arbitrary")]
impl arbitrary::Arbitrary<'_> for PTCCache {
    fn arbitrary(_u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(Self::default())
    }
}
