use super::*;
use crate::common::{
    get_attestation_participation_flag_indices, increase_balance, is_attestation_same_slot,
};
use crate::per_block_processing::errors::{BlockProcessingError, IntoWithIndex};
use safe_arith::SafeArith;
use types::consts::altair::{PARTICIPATION_FLAG_WEIGHTS, PROPOSER_WEIGHT, WEIGHT_DENOMINATOR};

pub mod base {
    use super::*;

    /// Validates each `Attestation` and updates the state, short-circuiting on an invalid object.
    ///
    /// Returns `Ok(())` if the validation and state updates completed successfully, otherwise returns
    /// an `Err` describing the invalid object or cause of failure.
    pub fn process_attestations<'a, E: EthSpec, I>(
        state: &mut BeaconState<E>,
        attestations: I,
        verify_signatures: VerifySignatures,
        ctxt: &mut ConsensusContext<E>,
        spec: &ChainSpec,
    ) -> Result<(), BlockProcessingError>
    where
        I: Iterator<Item = AttestationRef<'a, E>>,
    {
        // Ensure required caches are all built. These should be no-ops during regular operation.
        state.build_committee_cache(RelativeEpoch::Current, spec)?;
        state.build_committee_cache(RelativeEpoch::Previous, spec)?;
        initialize_epoch_cache(state, spec)?;
        initialize_progressive_balances_cache(state, spec)?;
        state.build_slashings_cache()?;

        let proposer_index = ctxt.get_proposer_index(state, spec)?;

        // Verify and apply each attestation.
        for (i, attestation) in attestations.enumerate() {
            verify_attestation_for_block_inclusion(
                state,
                attestation,
                ctxt,
                verify_signatures,
                spec,
            )
            .map_err(|e| e.into_with_index(i))?;

            let AttestationRef::Base(attestation) = attestation else {
                // Pending attestations have been deprecated in a altair, this branch should
                // never happen
                return Err(BlockProcessingError::PendingAttestationInElectra);
            };

            let pending_attestation = PendingAttestation {
                aggregation_bits: attestation.aggregation_bits.clone(),
                data: attestation.data.clone(),
                inclusion_delay: state.slot().safe_sub(attestation.data.slot)?.as_u64(),
                proposer_index,
            };

            if attestation.data.target.epoch == state.current_epoch() {
                state
                    .as_base_mut()?
                    .current_epoch_attestations
                    .push(pending_attestation)?;
            } else {
                state
                    .as_base_mut()?
                    .previous_epoch_attestations
                    .push(pending_attestation)?;
            }
        }

        Ok(())
    }
}

pub mod altair_gloas {
    use super::*;
    use crate::common::update_progressive_balances_cache::update_progressive_balances_on_attestation;

    pub fn process_attestation<E: EthSpec>(
        state: &mut BeaconState<E>,
        attestation: AttestationRef<E>,
        att_index: usize,
        ctxt: &mut ConsensusContext<E>,
        verify_signatures: VerifySignatures,
        spec: &ChainSpec,
    ) -> Result<(), BlockProcessingError> {
        if !state.fork_name_unchecked().gloas_enabled() {
            return altair::process_attestation(
                state,
                attestation,
                att_index,
                ctxt,
                verify_signatures,
                spec,
            );
        }

        gloas::process_attestation(state, attestation, att_index, ctxt, verify_signatures, spec)
    }

    pub fn process_attestations<'a, E: EthSpec, I>(
        state: &mut BeaconState<E>,
        attestations: I,
        verify_signatures: VerifySignatures,
        ctxt: &mut ConsensusContext<E>,
        spec: &ChainSpec,
    ) -> Result<(), BlockProcessingError>
    where
        I: Iterator<Item = AttestationRef<'a, E>>,
    {
        attestations.enumerate().try_for_each(|(i, attestation)| {
            process_attestation(state, attestation, i, ctxt, verify_signatures, spec)
        })
    }

    pub mod altair {
        use super::*;

        pub fn process_attestation<E: EthSpec>(
            state: &mut BeaconState<E>,
            attestation: AttestationRef<E>,
            att_index: usize,
            ctxt: &mut ConsensusContext<E>,
            verify_signatures: VerifySignatures,
            spec: &ChainSpec,
        ) -> Result<(), BlockProcessingError> {
            let proposer_index = ctxt.get_proposer_index(state, spec)?;
            let previous_epoch = ctxt.previous_epoch;
            let current_epoch = ctxt.current_epoch;

            let indexed_att = verify_attestation_for_block_inclusion(
                state,
                attestation,
                ctxt,
                verify_signatures,
                spec,
            )
            .map_err(|e| e.into_with_index(att_index))?;

            // Matching roots, participation flag indices
            let data = attestation.data();
            let inclusion_delay = state.slot().safe_sub(data.slot)?.as_u64();
            let participation_flag_indices =
                get_attestation_participation_flag_indices(state, data, inclusion_delay, spec)?;

            // Update epoch participation flags.
            let mut proposer_reward_numerator = 0;
            for index in indexed_att.attesting_indices_iter() {
                let index = *index as usize;

                let validator_effective_balance =
                    state.epoch_cache().get_effective_balance(index)?;
                let validator_slashed = state.slashings_cache().is_slashed(index);

                for (flag_index, &weight) in PARTICIPATION_FLAG_WEIGHTS.iter().enumerate() {
                    let epoch_participation = state.get_epoch_participation_mut(
                        data.target.epoch,
                        previous_epoch,
                        current_epoch,
                    )?;

                    if participation_flag_indices.contains(&flag_index) {
                        let validator_participation = epoch_participation
                            .get_mut(index)
                            .ok_or(BeaconStateError::ParticipationOutOfBounds(index))?;

                        if !validator_participation.has_flag(flag_index)? {
                            validator_participation.add_flag(flag_index)?;
                            proposer_reward_numerator
                                .safe_add_assign(state.get_base_reward(index)?.safe_mul(weight)?)?;

                            update_progressive_balances_on_attestation(
                                state,
                                data.target.epoch,
                                flag_index,
                                validator_effective_balance,
                                validator_slashed,
                            )?;
                        }
                    }
                }
            }

            let proposer_reward_denominator = WEIGHT_DENOMINATOR
                .safe_sub(PROPOSER_WEIGHT)?
                .safe_mul(WEIGHT_DENOMINATOR)?
                .safe_div(PROPOSER_WEIGHT)?;
            let proposer_reward =
                proposer_reward_numerator.safe_div(proposer_reward_denominator)?;
            increase_balance(state, proposer_index as usize, proposer_reward)?;
            Ok(())
        }
    }

    pub mod gloas {
        use super::*;

        pub fn process_attestation<E: EthSpec>(
            state: &mut BeaconState<E>,
            attestation: AttestationRef<E>,
            att_index: usize,
            ctxt: &mut ConsensusContext<E>,
            verify_signatures: VerifySignatures,
            spec: &ChainSpec,
        ) -> Result<(), BlockProcessingError> {
            let proposer_index = ctxt.get_proposer_index(state, spec)?;
            let previous_epoch = ctxt.previous_epoch;
            let current_epoch = ctxt.current_epoch;

            let indexed_att = verify_attestation_for_block_inclusion(
                state,
                attestation,
                ctxt,
                verify_signatures,
                spec,
            )
            .map_err(|e| e.into_with_index(att_index))?;

            // Matching roots, participation flag indices
            let data = attestation.data();
            let inclusion_delay = state.slot().safe_sub(data.slot)?.as_u64();
            let participation_flag_indices =
                get_attestation_participation_flag_indices(state, data, inclusion_delay, spec)?;

            // [New in EIP-7732]
            let current_epoch_target = data.target.epoch == state.current_epoch();
            let slot_mod = data
                .slot
                .as_usize()
                .safe_rem(E::slots_per_epoch() as usize)?;
            let payment_index = if current_epoch_target {
                (E::slots_per_epoch() as usize).safe_add(slot_mod)?
            } else {
                slot_mod
            };
            // Accumulate weight for same-slot attestations
            let mut accumulated_weight = 0;

            // Update epoch participation flags.
            let mut proposer_reward_numerator = 0;
            for index in indexed_att.attesting_indices_iter() {
                let index = *index as usize;

                let validator_effective_balance =
                    state.epoch_cache().get_effective_balance(index)?;
                let validator_slashed = state.slashings_cache().is_slashed(index);

                // [New in EIP7732]
                // For same-slot attestations, check if we're setting any new flags
                // If we are, this validator hasn't contributed to this slot's quorum yet
                let mut will_set_new_flag = false;

                for (flag_index, &weight) in PARTICIPATION_FLAG_WEIGHTS.iter().enumerate() {
                    let epoch_participation = state.get_epoch_participation_mut(
                        data.target.epoch,
                        previous_epoch,
                        current_epoch,
                    )?;

                    if participation_flag_indices.contains(&flag_index) {
                        let validator_participation = epoch_participation
                            .get_mut(index)
                            .ok_or(BeaconStateError::ParticipationOutOfBounds(index))?;

                        if !validator_participation.has_flag(flag_index)? {
                            validator_participation.add_flag(flag_index)?;
                            proposer_reward_numerator
                                .safe_add_assign(state.get_base_reward(index)?.safe_mul(weight)?)?;
                            will_set_new_flag = true;

                            update_progressive_balances_on_attestation(
                                state,
                                data.target.epoch,
                                flag_index,
                                validator_effective_balance,
                                validator_slashed,
                            )?;
                        }
                    }
                }

                // Check that payment_index is valid and get payment amount
                let builder_payments = state.builder_pending_payments_mut()?;
                let payment_amount = builder_payments
                    .get(payment_index)
                    .ok_or(BlockProcessingError::BuilderPaymentIndexOutOfBounds(
                        payment_index,
                    ))?
                    .withdrawal
                    .amount;

                // Collect validators for Gloas builder payment processing
                // We will only add weight for same-slot attestations when any new flag is set
                // This ensures each validator contributes exactly once per slot
                if will_set_new_flag && is_attestation_same_slot(state, data)? && payment_amount > 0
                {
                    accumulated_weight.safe_add_assign(validator_effective_balance)?;
                }
            }

            let proposer_reward_denominator = WEIGHT_DENOMINATOR
                .safe_sub(PROPOSER_WEIGHT)?
                .safe_mul(WEIGHT_DENOMINATOR)?
                .safe_div(PROPOSER_WEIGHT)?;
            let proposer_reward =
                proposer_reward_numerator.safe_div(proposer_reward_denominator)?;
            increase_balance(state, proposer_index as usize, proposer_reward)?;

            // Update builder payment weight
            if accumulated_weight > 0 {
                let builder_payments = state.builder_pending_payments_mut()?;
                let payment = builder_payments.get_mut(payment_index).ok_or(
                    BlockProcessingError::BuilderPaymentIndexOutOfBounds(payment_index),
                )?;
                payment.weight.safe_add_assign(accumulated_weight)?;
            }

            Ok(())
        }
    }
}
