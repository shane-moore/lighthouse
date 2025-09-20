use crate::duties_service::DutiesService;
use beacon_node_fallback::BeaconNodeFallback;
use eth2::types::PtcDuty;
use futures::future::join_all;
use logging::crit;
use slot_clock::SlotClock;
use std::ops::Deref;
use std::sync::Arc;
use task_executor::TaskExecutor;
use tokio::time::{Duration, sleep};
use tracing::{error, info, trace, warn};
use types::{ChainSpec, EthSpec, PayloadAttestationData, PayloadAttestationMessage, Slot};
use validator_metrics;
use validator_store::ValidatorStore;

/// Builds a `PayloadAttestationService`.
#[derive(Default)]
pub struct PayloadAttestationServiceBuilder<S: ValidatorStore, T: SlotClock + 'static> {
    duties_service: Option<Arc<DutiesService<S, T>>>,
    validator_store: Option<Arc<S>>,
    slot_clock: Option<T>,
    beacon_nodes: Option<Arc<BeaconNodeFallback<T>>>,
    executor: Option<TaskExecutor>,
    chain_spec: Option<Arc<ChainSpec>>,
}

impl<S: ValidatorStore + 'static, T: SlotClock + 'static> PayloadAttestationServiceBuilder<S, T> {
    pub fn new() -> Self {
        Self {
            duties_service: None,
            validator_store: None,
            slot_clock: None,
            beacon_nodes: None,
            executor: None,
            chain_spec: None,
        }
    }

    pub fn duties_service(mut self, service: Arc<DutiesService<S, T>>) -> Self {
        self.duties_service = Some(service);
        self
    }

    pub fn validator_store(mut self, store: Arc<S>) -> Self {
        self.validator_store = Some(store);
        self
    }

    pub fn slot_clock(mut self, slot_clock: T) -> Self {
        self.slot_clock = Some(slot_clock);
        self
    }

    pub fn beacon_nodes(mut self, beacon_nodes: Arc<BeaconNodeFallback<T>>) -> Self {
        self.beacon_nodes = Some(beacon_nodes);
        self
    }

    pub fn executor(mut self, executor: TaskExecutor) -> Self {
        self.executor = Some(executor);
        self
    }

    pub fn chain_spec(mut self, chain_spec: Arc<ChainSpec>) -> Self {
        self.chain_spec = Some(chain_spec);
        self
    }

    pub fn build(self) -> Result<PayloadAttestationService<S, T>, String> {
        Ok(PayloadAttestationService {
            inner: Arc::new(Inner {
                duties_service: self
                    .duties_service
                    .ok_or("Cannot build PayloadAttestationService without duties_service")?,
                validator_store: self
                    .validator_store
                    .ok_or("Cannot build PayloadAttestationService without validator_store")?,
                slot_clock: self
                    .slot_clock
                    .ok_or("Cannot build PayloadAttestationService without slot_clock")?,
                beacon_nodes: self
                    .beacon_nodes
                    .ok_or("Cannot build PayloadAttestationService without beacon_nodes")?,
                executor: self
                    .executor
                    .ok_or("Cannot build PayloadAttestationService without executor")?,
                chain_spec: self
                    .chain_spec
                    .ok_or("Cannot build PayloadAttestationService without chain_spec")?,
            }),
        })
    }
}

/// Helper to minimise `Arc` usage.
pub struct Inner<S, T> {
    duties_service: Arc<DutiesService<S, T>>,
    validator_store: Arc<S>,
    slot_clock: T,
    beacon_nodes: Arc<BeaconNodeFallback<T>>,
    executor: TaskExecutor,
    chain_spec: Arc<ChainSpec>,
}

pub struct PayloadAttestationService<S, T> {
    inner: Arc<Inner<S, T>>,
}

impl<S, T> Clone for PayloadAttestationService<S, T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S, T> Deref for PayloadAttestationService<S, T> {
    type Target = Inner<S, T>;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl<S: ValidatorStore + 'static, T: SlotClock + 'static> PayloadAttestationService<S, T> {
    /// Starts the service which periodically produces payload attestations.
    pub fn start_update_service(self, spec: &ChainSpec) -> Result<(), String> {
        if self.duties_service.disable_attesting {
            info!("Payload attestation service disabled");
            return Ok(());
        }

        let slot_duration = Duration::from_secs(spec.seconds_per_slot);
        let duration_to_next_slot = self
            .slot_clock
            .duration_to_next_slot()
            .ok_or("Unable to determine duration to next slot")?;

        info!(
            next_update_millis = duration_to_next_slot.as_millis(),
            "Payload attestation service started"
        );

        let executor = self.executor.clone();

        let interval_fut = async move {
            loop {
                if let Some(duration_to_next_slot) = self.slot_clock.duration_to_next_slot() {
                    let payload_attestation_delay = slot_duration * 3 / 4;

                    sleep(duration_to_next_slot + payload_attestation_delay).await;

                    // Check if we've reached the Gloas fork epoch before proceeding
                    if self.chain_spec.is_gloas_scheduled() {
                        let Some(current_slot) = self.slot_clock.now() else {
                            error!("Unable to read slot clock");
                            sleep(slot_duration).await;
                            continue;
                        };

                        let current_epoch = current_slot.epoch(S::E::slots_per_epoch());

                        let Some(gloas_fork_epoch) = self.chain_spec.gloas_fork_epoch else {
                            // Gloas fork epoch not configured, should not reach here
                            warn!("Gloas fork is scheduled but no fork epoch configured");
                            break;
                        };

                        if current_epoch < gloas_fork_epoch {
                            // Wait until the next slot and check again
                            continue;
                        }
                    } else {
                        // Gloas fork not scheduled, skip payload attestation duties
                        continue;
                    }

                    if let Err(e) = self.spawn_payload_attestation_tasks(slot_duration) {
                        crit!(error = e, "Failed to spawn payload attestation tasks")
                    } else {
                        trace!("Spawned payload attestation tasks");
                    }
                } else {
                    error!("Failed to read slot clock");
                    // If we can't read the slot clock, just wait another slot.
                    sleep(slot_duration).await;
                    continue;
                }
            }
        };

        executor.spawn(interval_fut, "payload_attestation_service");
        Ok(())
    }

    /// For each PTC duty at the current slot, spawn a new task that creates, signs and uploads
    /// the payload attestation message to the beacon node.
    fn spawn_payload_attestation_tasks(&self, _slot_duration: Duration) -> Result<(), String> {
        let slot = self.slot_clock.now().ok_or("Failed to read slot clock")?;

        // Get PTC duties for this slot
        let duties = self.duties_service.get_ptc_duties_for_slot(slot);

        // Spawn a single  task for all PTC duties in this slot
        self.executor.spawn_ignoring_error(
            self.clone().publish_payload_attestations(slot, duties),
            "payload_attestation_batch_publish",
        );

        Ok(())
    }

    /// Downloads `PayloadAttestationData`, signs them, and publishes them to the BN as `PayloadAttestationMessage`s
    /// for all PTC duties in a slot.
    async fn publish_payload_attestations(
        self,
        slot: Slot,
        duties: Vec<PtcDuty>,
    ) -> Result<(), ()> {
        let _payload_attestations_timer = validator_metrics::start_timer_vec(
            &validator_metrics::PAYLOAD_ATTESTATION_SERVICE_TIMES,
            &[validator_metrics::PAYLOAD_ATTESTATIONS],
        );

        if duties.is_empty() {
            return Ok(());
        }

        // Step 1: Single GET request for payload attestation data (shared by all validators)
        let payload_attestation_data = self
            .beacon_nodes
            .first_success(|beacon_node| async move {
                let _timer = validator_metrics::start_timer_vec(
                    &validator_metrics::PAYLOAD_ATTESTATION_SERVICE_TIMES,
                    &[validator_metrics::PAYLOAD_ATTESTATION_HTTP_GET],
                );

                beacon_node
                    .get_validator_payload_attestation_data(slot)
                    .await
                    .map_err(|e| format!("Failed to get payload attestation data: {:?}", e))
                    .map(|result| result.data().clone())
            })
            .await
            .map_err(move |e| {
                crit!(
                    error = format!("{:?}", e.to_string()),
                    slot = slot.as_u64(),
                    "Error during payload attestation data retrieval"
                )
            })?;

        // Step 2: Sign all attestations in parallel
        let signing_futures = duties.iter().map(|duty| {
            let service = self.clone();
            let payload_attestation_data = payload_attestation_data.clone();

            async move {
                // Ensure that the payload attestation data matches the duty slot.
                if duty.slot != payload_attestation_data.slot {
                    crit!(
                        validator = ?duty.pubkey,
                        duty_slot = %duty.slot,
                        attestation_slot = %payload_attestation_data.slot,
                        "Inconsistent validator duty slot during payload attestation signing"
                    );
                    return None;
                }

                match service
                    .sign_payload_attestation_data(&duty.pubkey, &payload_attestation_data, slot)
                    .await
                {
                    Ok(signature) => Some(PayloadAttestationMessage {
                        validator_index: duty.validator_index,
                        data: payload_attestation_data,
                        signature,
                    }),
                    Err(e) => {
                        crit!(
                            slot = slot.as_u64(),
                            validator_index = duty.validator_index,
                            error = e,
                            "Failed to sign payload attestation"
                        );
                        None
                    }
                }
            }
        });

        // Execute all signing futures in parallel
        let signed_attestations: Vec<PayloadAttestationMessage> = join_all(signing_futures)
            .await
            .into_iter()
            .flatten()
            .collect();

        if signed_attestations.is_empty() {
            warn!(
                slot = slot.as_u64(),
                num_duties = duties.len(),
                "No payload attestations were signed successfully"
            );
            return Err(());
        }

        // Step 3: Single batched POST request to beacon node of `PayloadAttestationMessage`
        let fork_name = self.chain_spec.fork_name_at_slot::<S::E>(slot);
        self.beacon_nodes
            .first_success(|beacon_node| {
                let signed_attestations = signed_attestations.clone();
                async move {
                    let _timer = validator_metrics::start_timer_vec(
                        &validator_metrics::PAYLOAD_ATTESTATION_SERVICE_TIMES,
                        &[validator_metrics::PAYLOAD_ATTESTATION_HTTP_POST],
                    );

                    beacon_node
                        .post_beacon_pool_payload_attestations(signed_attestations, fork_name)
                        .await
                }
            })
            .await
            .map_err(move |e| {
                crit!(
                    error = format!("{:?}", e.to_string()),
                    slot = slot.as_u64(),
                    "Error during payload attestation publishing"
                )
            })?;

        info!(
            count =  signed_attestations.len(),
            slot = slot.as_u64(),
            payload_present = payload_attestation_data.payload_present,
            blob_data_available = payload_attestation_data.blob_data_available,
            beacon_block_root = ?payload_attestation_data.beacon_block_root,
            "Successfully published payload attestations"
        );

        Ok(())
    }

    /// Sign payload attestation data according to the Gloas specification.
    ///
    /// This creates a `PayloadAttestationMessage` and uses the validator store to sign it
    async fn sign_payload_attestation_data(
        &self,
        validator_pubkey: &types::PublicKeyBytes,
        attestation_data: &PayloadAttestationData,
        _slot: Slot,
    ) -> Result<types::AggregateSignature, String> {
        // Create a PayloadAttestationMessage with an empty signature for signing
        let validator_index = self
            .validator_store
            .validator_index(validator_pubkey)
            .ok_or_else(|| format!("Unknown validator: {:?}", validator_pubkey))?;

        let mut payload_attestation_message =
            PayloadAttestationMessage::empty_for_signing(validator_index, attestation_data.clone());

        // Use the validator store to sign the message
        self.validator_store
            .sign_payload_attestation_message(*validator_pubkey, &mut payload_attestation_message)
            .await
            .map_err(|e| format!("Failed to sign payload attestation: {:?}", e))?;

        Ok(payload_attestation_message.signature)
    }
}
