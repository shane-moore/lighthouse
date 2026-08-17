use crate::duties_service::DutiesService;
use beacon_node_fallback::{
    ApiTopic, BeaconNodeFallback,
    beacon_head_monitor::{HeadEvent, head_event_or_deadline},
};
use bls::PublicKeyBytes;
use eth2::types::BlockId;
use futures::StreamExt;
use futures::future::FutureExt;
use logging::crit;
use slot_clock::SlotClock;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use task_executor::TaskExecutor;
use tokio::sync::{Mutex, broadcast};
use tokio::time::{Duration, Instant, sleep, sleep_until};
use tracing::{Instrument, debug, error, info, info_span, instrument, trace, warn};
use types::{
    ChainSpec, EthSpec, Hash256, Slot, SyncCommitteeSubscription, SyncContributionData, SyncDuty,
    SyncSelectionProof, SyncSubnetId,
};
use validator_store::{ContributionToSign, SyncMessageToSign, ValidatorStore};

pub const SUBSCRIPTION_LOOKAHEAD_EPOCHS: u64 = 4;

/// Duration from now until `due` past the start of `slot`, or `None` if `slot` is not the
/// current slot.
fn delay_until_slot_offset<T: SlotClock>(
    slot_clock: &T,
    slot: Slot,
    due: Duration,
) -> Option<Duration> {
    let now = slot_clock.now_duration()?;
    if slot_clock.slot_of(now)? != slot {
        return None;
    }
    let due_at = slot_clock.start_of(slot)?.checked_add(due)?;
    Some(due_at.saturating_sub(now))
}

/// The next slot and the duration until it starts, derived from a single clock read so the
/// two values always describe the same slot.
fn next_slot_with_duration<T: SlotClock>(slot_clock: &T) -> Option<(Slot, Duration)> {
    let now = slot_clock.now_duration()?;
    let next_slot = slot_clock.slot_of(now)? + 1;
    let duration_to_next_slot = slot_clock.start_of(next_slot)?.saturating_sub(now);
    Some((next_slot, duration_to_next_slot))
}

pub struct SyncCommitteeService<S: ValidatorStore, T: SlotClock + 'static> {
    inner: Arc<Inner<S, T>>,
}

impl<S: ValidatorStore, T: SlotClock + 'static> Clone for SyncCommitteeService<S, T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: ValidatorStore, T: SlotClock + 'static> Deref for SyncCommitteeService<S, T> {
    type Target = Inner<S, T>;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

pub struct Inner<S: ValidatorStore, T: SlotClock + 'static> {
    duties_service: Arc<DutiesService<S, T>>,
    validator_store: Arc<S>,
    slot_clock: T,
    beacon_nodes: Arc<BeaconNodeFallback<T>>,
    executor: TaskExecutor,
    head_monitor_rx: Mutex<Option<broadcast::Receiver<HeadEvent>>>,
    /// Boolean to track whether the service has posted subscriptions to the BN at least once.
    ///
    /// This acts as a latch that fires once upon start-up, and then never again.
    first_subscription_done: AtomicBool,
}

impl<S: ValidatorStore + 'static, T: SlotClock + 'static> SyncCommitteeService<S, T> {
    pub fn new(
        duties_service: Arc<DutiesService<S, T>>,
        validator_store: Arc<S>,
        slot_clock: T,
        beacon_nodes: Arc<BeaconNodeFallback<T>>,
        executor: TaskExecutor,
        head_monitor_rx: Option<broadcast::Receiver<HeadEvent>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                duties_service,
                validator_store,
                slot_clock,
                beacon_nodes,
                executor,
                head_monitor_rx: Mutex::new(head_monitor_rx),
                first_subscription_done: AtomicBool::new(false),
            }),
        }
    }

    /// Check if the Altair fork has been activated and therefore sync duties should be performed.
    ///
    /// Slot clock errors are mapped to `false`.
    fn altair_fork_activated(&self) -> bool {
        self.duties_service
            .spec
            .altair_fork_epoch
            .and_then(|fork_epoch| {
                let current_epoch = self.slot_clock.now()?.epoch(S::E::slots_per_epoch());
                Some(current_epoch >= fork_epoch)
            })
            .unwrap_or(false)
    }

    pub fn start_update_service(self, spec: &ChainSpec) -> Result<(), String> {
        if self.duties_service.disable_attesting {
            info!("Sync committee service disabled");
            return Ok(());
        }

        let slot_duration = spec.get_slot_duration();
        let duration_to_next_slot = self
            .slot_clock
            .duration_to_next_slot()
            .ok_or("Unable to determine duration to next slot")?;

        info!(
            next_update_millis = duration_to_next_slot.as_millis(),
            "Sync committee service started"
        );

        let executor = self.executor.clone();

        let interval_fut = async move {
            let mut head_monitor_rx = self.head_monitor_rx.lock().await.take();
            let mut last_processed_slot: Option<Slot> = None;
            loop {
                let Some((next_slot, duration_to_next_slot)) =
                    next_slot_with_duration(&self.slot_clock)
                else {
                    error!("Failed to read slot clock");
                    sleep(slot_duration).await;
                    continue;
                };

                // Wait for the sync message due point of the next slot, or a head event for the
                // current slot, whichever comes first.
                let sync_message_due = self
                    .duties_service
                    .spec
                    .get_sync_message_due_at_slot::<S::E>(next_slot);
                let head_event = head_event_or_deadline(
                    &mut head_monitor_rx,
                    &self.slot_clock,
                    duration_to_next_slot + sync_message_due,
                )
                .await;

                // Take the slot from the trigger itself rather than re-reading the clock, so a
                // head event arriving at the end of a slot is never attributed to the next slot.
                let (current_slot, head_event_root) = match head_event {
                    Some(event) => (event.slot, Some(event.beacon_block_root)),
                    None => (next_slot, None),
                };

                if last_processed_slot.is_some_and(|last_slot| current_slot <= last_slot) {
                    debug!(%current_slot, "Sync message slot already processed");
                    continue;
                }

                // Do nothing if the Altair fork has not yet occurred.
                if !self.altair_fork_activated() {
                    continue;
                }

                self.spawn_contribution_tasks(current_slot, head_event_root)
                    .await;
                last_processed_slot = Some(current_slot);

                // Do subscriptions for future slots/epochs.
                self.spawn_subscription_tasks();
            }
        };

        executor.spawn(interval_fut, "sync_committee_service");
        Ok(())
    }

    async fn spawn_contribution_tasks(&self, slot: Slot, mut head_event_root: Option<Hash256>) {
        let spec = &self.duties_service.spec;

        let mut slot_duties = self
            .duties_service
            .sync_duties
            .get_duties_for_slot::<S::E>(slot, spec);

        // If a head event triggered us before the duties were computed, wait until the sync
        // message deadline and check for duties once more.
        if slot_duties.is_none() && head_event_root.is_some() {
            let Some(duration_to_deadline) = delay_until_slot_offset(
                &self.slot_clock,
                slot,
                spec.get_sync_message_due_at_slot::<S::E>(slot),
            ) else {
                debug!(%slot, "Skipping sync committee tasks for expired slot");
                return;
            };
            sleep(duration_to_deadline).await;

            slot_duties = self
                .duties_service
                .sync_duties
                .get_duties_for_slot::<S::E>(slot, spec);

            // The head may have changed while sleeping, so discard the event root and fall
            // back to a fresh head lookup below.
            head_event_root = None;
        }

        let Some(slot_duties) = slot_duties else {
            debug!(%slot, "No duties known for slot");
            return;
        };

        if slot_duties.duties.is_empty() {
            debug!(%slot, "No local validators in current sync committee");
            return;
        }

        // If a validator needs to publish a sync aggregate, they must do so at 2/3
        // through the slot. This delay triggers at this time
        let Some(contribution_delay) =
            delay_until_slot_offset(&self.slot_clock, slot, spec.get_contribution_message_due())
        else {
            debug!(%slot, "Skipping sync committee tasks for expired slot");
            return;
        };
        // Messages past the contribution deadline can no longer be aggregated, so a trigger
        // this late (a head event for the slot already in progress at startup) is skipped.
        if contribution_delay.is_zero() {
            debug!(%slot, "Skipping sync committee tasks, contribution deadline passed");
            return;
        }
        let aggregate_production_instant = Instant::now() + contribution_delay;

        debug!(
            %slot,
            from_head_monitor = head_event_root.is_some(),
            "Starting sync committee message production"
        );

        let block_root = if let Some(block_root) = head_event_root {
            // The head monitor only forwards non-optimistic heads, so the event root can be
            // used directly.
            block_root
        } else {
            // Fetch `block_root` with non optimistic execution for `SyncCommitteeContribution`.
            let response = self
                .beacon_nodes
                .first_success(
                    |beacon_node| async move {
                        match beacon_node.get_beacon_blocks_root(BlockId::Head).await {
                            Ok(Some(block)) if block.execution_optimistic == Some(false) => {
                                Ok(block)
                            }
                            Ok(Some(_)) => {
                                Err(format!("To sign sync committee messages for slot {slot} a non-optimistic head block is required"))
                            }
                            Ok(None) => Err(format!("No block root found for slot {}", slot)),
                            Err(e) => Err(e.to_string()),
                        }
                    },
                )
                .await;

            match response {
                Ok(block) => block.data.root,
                Err(errs) => {
                    warn!(
                        errors = errs.to_string(),
                        %slot,
                        "Refusing to sign sync committee messages for an optimistic head block or \
                        a block head with unknown optimistic status"
                    );
                    return;
                }
            }
        };

        // Spawn one task to publish all of the sync committee signatures.
        let validator_duties = slot_duties.duties;
        let service = self.clone();
        self.inner.executor.spawn(
            async move {
                service
                    .publish_sync_committee_signatures(slot, block_root, validator_duties)
                    .map(|_| ())
                    .await
            }
            .instrument(info_span!("lh_sync_committee_signature_publish", %slot)),
            "sync_committee_signature_publish",
        );

        let aggregators = slot_duties.aggregators;
        let service = self.clone();
        self.inner.executor.spawn(
            async move {
                service
                    .publish_sync_committee_aggregates(
                        slot,
                        block_root,
                        aggregators,
                        aggregate_production_instant,
                    )
                    .map(|_| ())
                    .await
            }
            .instrument(info_span!("lh_sync_committee_aggregate_publish", %slot)),
            "sync_committee_aggregate_publish",
        );

        trace!("Spawned sync contribution tasks");
    }

    /// Publish sync committee signatures.
    #[instrument(skip_all, fields(%slot, ?beacon_block_root))]
    async fn publish_sync_committee_signatures(
        &self,
        slot: Slot,
        beacon_block_root: Hash256,
        validator_duties: Vec<SyncDuty>,
    ) -> Result<(), ()> {
        let messages_to_sign: Vec<_> = validator_duties
            .iter()
            .map(|duty| SyncMessageToSign {
                slot,
                beacon_block_root,
                validator_index: duty.validator_index,
                pubkey: duty.pubkey,
            })
            .collect();

        let signature_stream = self
            .validator_store
            .sign_sync_committee_signatures(messages_to_sign);
        tokio::pin!(signature_stream);

        while let Some(result) = signature_stream.next().await {
            match result {
                Ok(committee_signatures) if !committee_signatures.is_empty() => {
                    let committee_signatures = &committee_signatures;
                    match self
                        .beacon_nodes
                        .request(ApiTopic::SyncCommittee, |beacon_node| async move {
                            beacon_node
                                .post_beacon_pool_sync_committee_signatures(committee_signatures)
                                .await
                        })
                        .instrument(info_span!(
                            "publish_sync_signatures",
                            count = committee_signatures.len()
                        ))
                        .await
                    {
                        Ok(()) => info!(
                            count = committee_signatures.len(),
                            head_block = ?beacon_block_root,
                            %slot,
                            "Successfully published sync committee messages"
                        ),
                        Err(e) => error!(
                            %slot,
                            error = %e,
                            "Unable to publish sync committee messages"
                        ),
                    }
                }
                Err(e) => {
                    crit!(%slot, error = ?e, "Failed to sign sync committee signatures");
                }
                _ => {}
            }
        }

        Ok(())
    }

    async fn publish_sync_committee_aggregates(
        &self,
        slot: Slot,
        beacon_block_root: Hash256,
        aggregators: HashMap<SyncSubnetId, Vec<(u64, PublicKeyBytes, SyncSelectionProof)>>,
        aggregate_instant: Instant,
    ) {
        for (subnet_id, subnet_aggregators) in aggregators {
            let service = self.clone();
            self.inner.executor.spawn(
                async move {
                    service
                        .publish_sync_committee_aggregate_for_subnet(
                            slot,
                            beacon_block_root,
                            subnet_id,
                            subnet_aggregators,
                            aggregate_instant,
                        )
                        .map(|_| ())
                        .await
                }
                .instrument(info_span!("lh_publish_sync_committee_aggregate_for_subnet", %slot, ?beacon_block_root, %subnet_id)),
                "sync_committee_aggregate_publish_subnet",
            );
        }
    }

    async fn publish_sync_committee_aggregate_for_subnet(
        &self,
        slot: Slot,
        beacon_block_root: Hash256,
        subnet_id: SyncSubnetId,
        subnet_aggregators: Vec<(u64, PublicKeyBytes, SyncSelectionProof)>,
        aggregate_instant: Instant,
    ) -> Result<(), ()> {
        sleep_until(aggregate_instant).await;

        let contribution = &self
            .beacon_nodes
            .first_success(|beacon_node| async move {
                let sync_contribution_data = SyncContributionData {
                    slot,
                    beacon_block_root,
                    subcommittee_index: subnet_id.into(),
                };

                beacon_node
                    .get_validator_sync_committee_contribution(&sync_contribution_data)
                    .await
            })
            .instrument(info_span!("fetch_sync_contribution"))
            .await
            .map_err(|e| {
                crit!(
                    %slot,
                    ?beacon_block_root,
                    error = %e,
                    "Failed to produce sync contribution"
                );
            })?
            .ok_or_else(|| {
                crit!(%slot, ?beacon_block_root, "No aggregate contribution found");
            })?
            .data;

        let contributions_to_sign: Vec<_> = subnet_aggregators
            .into_iter()
            .map(
                |(aggregator_index, aggregator_pk, selection_proof)| ContributionToSign {
                    aggregator_index,
                    aggregator_pubkey: aggregator_pk,
                    contribution: contribution.clone(),
                    selection_proof,
                },
            )
            .collect();

        let contribution_stream = self
            .validator_store
            .sign_sync_committee_contributions(contributions_to_sign);
        tokio::pin!(contribution_stream);

        while let Some(result) = contribution_stream.next().await {
            match result {
                Ok(signed_contributions) if !signed_contributions.is_empty() => {
                    let signed_contributions = &signed_contributions;
                    // Publish to the beacon node.
                    match self
                        .beacon_nodes
                        .first_success(|beacon_node| async move {
                            beacon_node
                                .post_validator_contribution_and_proofs(signed_contributions)
                                .await
                        })
                        .instrument(info_span!(
                            "publish_sync_contributions",
                            count = signed_contributions.len()
                        ))
                        .await
                    {
                        Ok(()) => info!(
                            subnet = %subnet_id,
                            beacon_block_root = %beacon_block_root,
                            num_signers = contribution.aggregation_bits.num_set_bits(),
                            %slot,
                            "Successfully published sync contributions"
                        ),
                        Err(e) => error!(
                            %slot,
                            error = %e,
                            "Unable to publish signed contributions and proofs"
                        ),
                    }
                }
                Err(e) => {
                    crit!(%slot, error = ?e, "Failed to sign sync committee contributions");
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn spawn_subscription_tasks(&self) {
        let service = self.clone();

        self.inner.executor.spawn(
            async move {
                service.publish_subscriptions().await.unwrap_or_else(|e| {
                    error!(
                        error = ?e,
                        "Error publishing subscriptions"
                    )
                });
            },
            "sync_committee_subscription_publish",
        );
    }

    async fn publish_subscriptions(self) -> Result<(), String> {
        let spec = &self.duties_service.spec;
        let slot = self.slot_clock.now().ok_or("Failed to read slot clock")?;

        let mut duty_slots = vec![];
        let mut all_succeeded = true;

        // At the start of every epoch during the current period, re-post the subscriptions
        // to the beacon node. This covers the case where the BN has forgotten the subscriptions
        // due to a restart, or where the VC has switched to a fallback BN.
        let current_period = sync_period_of_slot::<S::E>(slot, spec)?;

        if !self.first_subscription_done.load(Ordering::Relaxed)
            || slot.as_u64() % S::E::slots_per_epoch() == 0
        {
            duty_slots.push((slot, current_period));
        }

        // Near the end of the current period, push subscriptions for the next period to the
        // beacon node. We aggressively push every slot in the lead-up, as this is the main way
        // that we want to ensure that the BN is subscribed (well in advance).
        let lookahead_slot = slot + SUBSCRIPTION_LOOKAHEAD_EPOCHS * S::E::slots_per_epoch();

        let lookahead_period = sync_period_of_slot::<S::E>(lookahead_slot, spec)?;

        if lookahead_period > current_period {
            duty_slots.push((lookahead_slot, lookahead_period));
        }

        if duty_slots.is_empty() {
            return Ok(());
        }

        // Collect subscriptions.
        let mut subscriptions = vec![];

        for (duty_slot, sync_committee_period) in duty_slots {
            debug!(%duty_slot, %slot, "Fetching subscription duties");
            match self
                .duties_service
                .sync_duties
                .get_duties_for_slot::<S::E>(duty_slot, spec)
            {
                Some(duties) => subscriptions.extend(subscriptions_from_sync_duties(
                    duties.duties,
                    sync_committee_period,
                    spec,
                )),
                None => {
                    debug!(
                        slot = %duty_slot,
                        "No duties for subscription"
                    );
                    all_succeeded = false;
                }
            }
        }

        if subscriptions.is_empty() {
            debug!(%slot, "No sync subscriptions to send");
            return Ok(());
        }

        // Post subscriptions to BN.
        debug!(
            count = subscriptions.len(),
            "Posting sync subscriptions to BN"
        );
        let subscriptions_slice = &subscriptions;

        for subscription in subscriptions_slice {
            debug!(
                validator_index = subscription.validator_index,
                validator_sync_committee_indices = ?subscription.sync_committee_indices,
                until_epoch = %subscription.until_epoch,
                "Subscription"
            );
        }

        if let Err(e) = self
            .beacon_nodes
            .request(ApiTopic::Subscriptions, |beacon_node| async move {
                beacon_node
                    .post_validator_sync_committee_subscriptions(subscriptions_slice)
                    .await
            })
            .await
        {
            error!(
                %slot,
                error = %e,
                "Unable to post sync committee subscriptions"
            );
            all_succeeded = false;
        }

        // Disable first-subscription latch once all duties have succeeded once.
        if all_succeeded {
            self.first_subscription_done.store(true, Ordering::Relaxed);
        }

        Ok(())
    }
}

fn sync_period_of_slot<E: EthSpec>(slot: Slot, spec: &ChainSpec) -> Result<u64, String> {
    slot.epoch(E::slots_per_epoch())
        .sync_committee_period(spec)
        .map_err(|e| format!("Error computing sync period: {:?}", e))
}

fn subscriptions_from_sync_duties(
    duties: Vec<SyncDuty>,
    sync_committee_period: u64,
    spec: &ChainSpec,
) -> impl Iterator<Item = SyncCommitteeSubscription> {
    let until_epoch = spec.epochs_per_sync_committee_period * (sync_committee_period + 1);
    duties
        .into_iter()
        .map(move |duty| SyncCommitteeSubscription {
            validator_index: duty.validator_index,
            sync_committee_indices: duty.validator_sync_committee_indices,
            until_epoch,
        })
}
