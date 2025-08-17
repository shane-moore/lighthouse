use types::{AttestationData, BeaconState, BeaconStateError, EthSpec};

/// Checks if the attestation was for the block proposed at the attestation slot.
///
/// Returns true if:
/// - The attestation is for slot 0 (genesis), OR
/// - The attestation's beacon_block_root matches the block actually proposed at that slot
///   AND it's different from the previous slot's block (indicating no skip)
pub fn is_attestation_same_slot<E: EthSpec>(
    state: &BeaconState<E>,
    data: &AttestationData,
) -> Result<bool, BeaconStateError> {
    if data.slot == 0 {
        return Ok(true);
    }

    let is_matching_block_root = data.beacon_block_root == *state.get_block_root(data.slot)?;
    let is_current_block_root = data.beacon_block_root != *state.get_block_root(data.slot - 1)?;

    Ok(is_matching_block_root && is_current_block_root)
}
