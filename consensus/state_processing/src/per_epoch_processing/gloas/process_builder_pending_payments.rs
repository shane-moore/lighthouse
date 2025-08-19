use crate::EpochProcessingError;
use safe_arith::SafeArith;
use types::{BeaconState, BuilderPendingPayment, ChainSpec, EthSpec, Vector};

/// TODO(EIP-7732): Add EF consensus-spec tests for `process_builder_pending_payments`
/// Currently blocked by EF consensus-spec-tests for Gloas not yet integrated.
pub fn process_builder_pending_payments<E: EthSpec>(
    state: &mut BeaconState<E>,
    spec: &ChainSpec,
) -> Result<(), EpochProcessingError> {
    let quorum = get_builder_payment_quorum_threshold(state, spec)?;

    // Collect qualifying payments
    let qualifying_payments = state
        .builder_pending_payments()?
        .iter()
        .take(E::slots_per_epoch() as usize)
        .filter(|payment| payment.weight > quorum)
        .cloned()
        .collect::<Vec<_>>();

    // Update `builder_pending_withdrawals` with qualifying `builder_pending_payments`
    qualifying_payments.into_iter().try_for_each(
        |payment| -> Result<(), EpochProcessingError> {
            let exit_queue_epoch =
                state.compute_exit_epoch_and_update_churn(payment.withdrawal.amount, spec)?;
            let withdrawable_epoch =
                exit_queue_epoch.safe_add(spec.min_validator_withdrawability_delay)?;

            let mut withdrawal = payment.withdrawal.clone();
            withdrawal.withdrawable_epoch = withdrawable_epoch;
            state.builder_pending_withdrawals_mut()?.push(withdrawal)?;
            Ok(())
        },
    )?;

    // Move remaining `builder_pending_payments` to start of list and set the rest to default
    let new_payments = state
        .builder_pending_payments()?
        .iter()
        .skip(E::slots_per_epoch() as usize)
        .cloned()
        .chain((0..E::slots_per_epoch() as usize).map(|_| BuilderPendingPayment::default()))
        .collect::<Vec<_>>();

    *state.builder_pending_payments_mut()? = Vector::new(new_payments)?;

    Ok(())
}

pub fn get_builder_payment_quorum_threshold<E: EthSpec>(
    state: &BeaconState<E>,
    spec: &ChainSpec,
) -> Result<u64, EpochProcessingError> {
    let total_active_balance = state.get_total_active_balance()?;

    let quorum = total_active_balance
        .safe_div(E::slots_per_epoch())?
        .safe_mul(spec.builder_payment_threshold_numerator)?;

    quorum
        .safe_div(spec.builder_payment_threshold_denominator)
        .map_err(EpochProcessingError::from)
}
