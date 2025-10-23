use safe_arith::SafeArith;
use types::{AttestationData, BeaconState, BeaconStateError, EthSpec};

/// Checks if the attestation was for the block proposed at the attestation slot.
pub fn is_attestation_same_slot<E: EthSpec>(
    state: &BeaconState<E>,
    data: &AttestationData,
) -> Result<bool, BeaconStateError> {
    if data.slot == 0 {
        return Ok(true);
    }

    let is_matching_block_root = &data.beacon_block_root == state.get_block_root(data.slot)?;
    let is_current_block_root =
        &data.beacon_block_root != state.get_block_root(data.slot.safe_sub(1)?)?;

    Ok(is_matching_block_root && is_current_block_root)
}
