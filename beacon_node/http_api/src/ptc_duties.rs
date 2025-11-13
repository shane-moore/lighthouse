//! Contains the handler for the `POST validator/duties/ptc/{epoch}` endpoint.

use beacon_chain::{BeaconChain, BeaconChainError, BeaconChainTypes};
use eth2::types::{self as api_types};
use slot_clock::SlotClock;
use types::{Epoch, EthSpec, Hash256, PtcDuty};

/// The struct that is returned to the requesting HTTP client.
type ApiDuties = api_types::DutiesResponse<Vec<PtcDuty>>;

/// Handles a request from the HTTP API for PTC duties.
pub fn ptc_duties<T: BeaconChainTypes>(
    request_epoch: Epoch,
    request_indices: &[u64],
    chain: &BeaconChain<T>,
) -> Result<ApiDuties, warp::reject::Rejection> {
    let current_epoch = chain
        .slot_clock
        .now_or_genesis()
        .map(|slot| slot.epoch(T::EthSpec::slots_per_epoch()))
        .ok_or(BeaconChainError::UnableToReadSlot)
        .map_err(warp_utils::reject::unhandled_error)?;

    // Determine what the current epoch would be if we fast-forward our system clock by
    // `MAXIMUM_GOSSIP_CLOCK_DISPARITY`.
    //
    // Most of the time, `tolerant_current_epoch` will be equal to `current_epoch`. However, during
    // the first `MAXIMUM_GOSSIP_CLOCK_DISPARITY` duration of the epoch `tolerant_current_epoch`
    // will equal `current_epoch + 1`
    let tolerant_current_epoch = if chain.slot_clock.is_prior_to_genesis().unwrap_or(true) {
        current_epoch
    } else {
        chain
            .slot_clock
            .now_with_future_tolerance(chain.spec.maximum_gossip_clock_disparity())
            .ok_or_else(|| {
                warp_utils::reject::custom_server_error("unable to read slot clock".into())
            })?
            .epoch(T::EthSpec::slots_per_epoch())
    };

    if request_epoch == current_epoch
        || request_epoch == current_epoch + 1
        || request_epoch == tolerant_current_epoch + 1
    {
        compute_ptc_duties(request_epoch, request_indices, chain)
    } else if request_epoch > current_epoch + 1 {
        Err(warp_utils::reject::custom_bad_request(format!(
            "request epoch {} is more than one epoch past the current epoch {}",
            request_epoch, current_epoch
        )))
    } else {
        // request_epoch < current_epoch, in fact we only allow `request_epoch == current_epoch-1` in this case
        compute_historic_ptc_duties(request_epoch, request_indices, chain)
    }
}

fn compute_ptc_duties<T: BeaconChainTypes>(
    request_epoch: Epoch,
    request_indices: &[u64],
    chain: &BeaconChain<T>,
) -> Result<ApiDuties, warp::reject::Rejection> {
    let (duties, dependent_root, execution_status) = chain
        .validator_ptc_duties(request_indices, request_epoch)
        .map_err(warp_utils::reject::unhandled_error)?;

    convert_to_api_response::<T>(
        duties,
        dependent_root,
        execution_status.is_optimistic_or_invalid(),
    )
}

/// Compute PTC duties by reading a `BeaconState` from disk, ignoring the ptc cache
fn compute_historic_ptc_duties<T: BeaconChainTypes>(
    _request_epoch: Epoch,
    _request_indices: &[u64],
    _chain: &BeaconChain<T>,
) -> Result<ApiDuties, warp::reject::Rejection> {
    // TODO(EIP-7732): add support for historic PTC duties after devnet-0
    // ideally, also after ptc cache PR has been upstreamed to sigp
    // https://github.com/shane-moore/lighthouse/pull/10
    todo!("Gloas historic ptc duties not implemented");
}

/// Convert internal PTC duties to API response format
fn convert_to_api_response<T: BeaconChainTypes>(
    duties: Vec<Option<PtcDuty>>,
    dependent_root: Hash256,
    execution_optimistic: bool,
) -> Result<ApiDuties, warp::reject::Rejection> {
    let data = duties.into_iter().flatten().collect::<Vec<_>>();

    Ok(api_types::DutiesResponse {
        dependent_root,
        execution_optimistic: Some(execution_optimistic),
        data,
    })
}
