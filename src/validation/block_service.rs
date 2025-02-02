//! Reference: lighthouse/validator_client/block_service.rs 

use crate::validation::beacon_node_fallback::{Errors, Error as FallbackError};
use crate::validation::{
    beacon_node_fallback::{BeaconNodeFallback, RequireSynced, OfflineOnFailure},
    graffiti_file::GraffitiFile,
};
use crate::validation::{http_metrics::metrics, validator_store::ValidatorStore, validator_store::Error as VSError};
use crate::validation::signing_method::Error as SigningError;
use environment::RuntimeContext;
use eth2::types::Graffiti;
use slog::{crit, debug, error, info, trace, warn};
use slot_clock::SlotClock;
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::mpsc;
use types::{
    AbstractExecPayload, BlindedPayload, BlockType, Epoch, EthSpec, FullPayload, PublicKeyBytes, Slot,
};

#[derive(Debug)]
pub enum BlockError {
    Recoverable(String),
    Irrecoverable(String),
    RandaoNotLeader,
    SignBlockNotLeader,
}

impl From<Errors<BlockError>> for BlockError {
    fn from(e: Errors<BlockError>) -> Self {
        if e.0.iter().any(|(_, error)| {
            matches!(
                error,
                FallbackError::RequestFailed(BlockError::Irrecoverable(_))
            )
        }) {
            BlockError::Irrecoverable(e.to_string())
        } else {
            BlockError::Recoverable(e.to_string())
        }
    }
}

/// Builds a `BlockService`.
pub struct BlockServiceBuilder<T, E: EthSpec> {
    validator_store: Option<Arc<ValidatorStore<T, E>>>,
    slot_clock: Option<Arc<T>>,
    beacon_nodes: Option<Arc<BeaconNodeFallback<T, E>>>,
    context: Option<RuntimeContext<E>>,
    graffiti: Option<Graffiti>,
    graffiti_file: Option<GraffitiFile>,
    private_tx_proposals: bool,
}

impl<T: SlotClock + 'static, E: EthSpec> BlockServiceBuilder<T, E> {
    pub fn new() -> Self {
        Self {
            validator_store: None,
            slot_clock: None,
            beacon_nodes: None,
            context: None,
            graffiti: None,
            graffiti_file: None,
            private_tx_proposals: false,
        }
    }

    pub fn validator_store(mut self, store: Arc<ValidatorStore<T, E>>) -> Self {
        self.validator_store = Some(store);
        self
    }

    pub fn slot_clock(mut self, slot_clock: T) -> Self {
        self.slot_clock = Some(Arc::new(slot_clock));
        self
    }

    pub fn beacon_nodes(mut self, beacon_nodes: Arc<BeaconNodeFallback<T, E>>) -> Self {
        self.beacon_nodes = Some(beacon_nodes);
        self
    }

    pub fn runtime_context(mut self, context: RuntimeContext<E>) -> Self {
        self.context = Some(context);
        self
    }

    pub fn graffiti(mut self, graffiti: Option<Graffiti>) -> Self {
        self.graffiti = graffiti;
        self
    }

    pub fn graffiti_file(mut self, graffiti_file: Option<GraffitiFile>) -> Self {
        self.graffiti_file = graffiti_file;
        self
    }

    pub fn private_tx_proposals(mut self, private_tx_proposals: bool) -> Self {
        self.private_tx_proposals = private_tx_proposals;
        self
    }

    pub fn build(self) -> Result<BlockService<T, E>, String> {
        Ok(BlockService {
            inner: Arc::new(Inner {
                validator_store: self
                    .validator_store
                    .ok_or("Cannot build BlockService without validator_store")?,
                slot_clock: self
                    .slot_clock
                    .ok_or("Cannot build BlockService without slot_clock")?,
                beacon_nodes: self
                    .beacon_nodes
                    .ok_or("Cannot build BlockService without beacon_node")?,
                context: self
                    .context
                    .ok_or("Cannot build BlockService without runtime_context")?,
                graffiti: self.graffiti,
                graffiti_file: self.graffiti_file,
                private_tx_proposals: self.private_tx_proposals,
            }),
        })
    }
}

/// Helper to minimise `Arc` usage.
pub struct Inner<T, E: EthSpec> {
    validator_store: Arc<ValidatorStore<T, E>>,
    slot_clock: Arc<T>,
    beacon_nodes: Arc<BeaconNodeFallback<T, E>>,
    context: RuntimeContext<E>,
    graffiti: Option<Graffiti>,
    graffiti_file: Option<GraffitiFile>,
    private_tx_proposals: bool,
}

/// Attempts to produce attestations for any block producer(s) at the start of the epoch.
pub struct BlockService<T, E: EthSpec> {
    inner: Arc<Inner<T, E>>,
}

impl<T, E: EthSpec> Clone for BlockService<T, E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T, E: EthSpec> Deref for BlockService<T, E> {
    type Target = Inner<T, E>;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

/// Notification from the duties service that we should try to produce a block.
pub struct BlockServiceNotification {
    pub slot: Slot,
    pub block_proposers: Vec<PublicKeyBytes>,
}

impl<T: SlotClock + 'static, E: EthSpec> BlockService<T, E> {
    pub fn start_update_service(
        self,
        mut notification_rx: mpsc::Receiver<BlockServiceNotification>,
    ) -> Result<(), String> {
        let log = self.context.log().clone();

        info!(log, "Block production service started");

        let executor = self.inner.context.executor.clone();

        executor.spawn(
            async move {
                while let Some(notif) = notification_rx.recv().await {
                    let service = self.clone();
                    service.do_update(notif).await.ok();
                }
                debug!(log, "Block service shutting down");
            },
            "block_service",
        );

        Ok(())
    }

    /// Attempt to produce a block for any block producers in the `ValidatorStore`.
    async fn do_update(&self, notification: BlockServiceNotification) -> Result<(), ()> {
        let log = self.context.log();
        let _timer =
            metrics::start_timer_vec(&metrics::BLOCK_SERVICE_TIMES, &[metrics::FULL_UPDATE]);

        let slot = self.slot_clock.now().ok_or_else(move || {
            crit!(log, "Duties manager failed to read slot clock");
        })?;

        if notification.slot != slot {
            warn!(
                log,
                "Skipping block production for expired slot";
                "current_slot" => slot.as_u64(),
                "notification_slot" => notification.slot.as_u64(),
                "info" => "Your machine could be overloaded"
            );
            return Ok(());
        }

        if slot == self.context.eth2_config.spec.genesis_slot {
            debug!(
                log,
                "Not producing block at genesis slot";
                "proposers" => format!("{:?}", notification.block_proposers),
            );
            return Ok(());
        }

        trace!(
            log,
            "Block service update started";
            "slot" => slot.as_u64()
        );

        let proposers = notification.block_proposers;

        if proposers.is_empty() {
            trace!(
                log,
                "No local block proposers for this slot";
                "slot" => slot.as_u64()
            )
        } else if proposers.len() > 1 {
            error!(
                log,
                "Multiple block proposers for this slot";
                "action" => "producing blocks for all proposers",
                "num_proposers" => proposers.len(),
                "slot" => slot.as_u64(),
            )
        }

        let private_tx_proposals = self.private_tx_proposals;
        let merge_slot = self
            .context
            .eth2_config
            .spec
            .bellatrix_fork_epoch
            .unwrap_or_else(Epoch::max_value)
            .start_slot(E::slots_per_epoch());
        for validator_pubkey in proposers {
            let service = self.clone();
            let log = log.clone();
            self.inner.context.executor.spawn(
                async move {
                    let publish_result = if private_tx_proposals && slot >= merge_slot {
                        let mut result = service.clone()
                            .publish_block::<BlindedPayload<E>>(slot, validator_pubkey)
                            .await;
                        match result.as_ref() {
                            Err(BlockError::Recoverable(e)) => {
                                error!(log, "Error whilst producing a blinded block, attempting to publish full block"; "error" => ?e);
                                result = service
                                    .publish_block::<FullPayload<E>>(slot, validator_pubkey)
                                    .await;
                            },
                            Err(BlockError::Irrecoverable(e))  => {
                                error!(log, "Error whilst producing a blinded block, cannot fallback because block was signed"; "error" => ?e);
                            },
                            _ => {},
                        };
                        result
                    } else {
                        service
                            .publish_block::<FullPayload<E>>(slot, validator_pubkey)
                            .await
                    };
                    if let Err(e) = publish_result {
                        match e {
                            BlockError::RandaoNotLeader => {
                                error!(log,
                                    "Error whilst producing block";
                                    "message" => ?e
                                );
                            },
                            BlockError::SignBlockNotLeader => {
                                info!(log,
                                    "Not a leader for proposing this block";
                                    "message" => ?e
                                );
                            },
                            _ => {
                                crit!(
                                    log,
                                    "Error whilst producing block";
                                    "message" => ?e
                                );
                            }
                        };
                    }
                },
                "block service",
            );
        }

        Ok(())
    }

    /// Produce a block at the given slot for validator_pubkey
    async fn publish_block<Payload: AbstractExecPayload<E>>(
        self,
        slot: Slot,
        validator_pubkey: PublicKeyBytes,
    ) -> Result<(), BlockError> {
        let log = self.context.log();
        let _timer =
            metrics::start_timer_vec(&metrics::BLOCK_SERVICE_TIMES, &[metrics::BEACON_BLOCK]);

        let current_slot = self.slot_clock.now().ok_or_else(|| {
            BlockError::Recoverable("Unable to determine current slot from clock".to_string())
        })?;

        let randao_reveal = self
            .validator_store
            .randao_reveal(validator_pubkey, slot.epoch(E::slots_per_epoch()))
            .await
            .map_err(|e| {
                match e {
                    VSError::UnableToSign(SigningError::NotLeader) => BlockError::RandaoNotLeader,
                    _ => BlockError::Recoverable(format!("Unable to produce randao reveal signature: {:?}", e))
                }
            })?
            .into();

        let graffiti = self
            .graffiti_file
            .clone()
            .and_then(|mut g| match g.load_graffiti(&validator_pubkey) {
                Ok(g) => g,
                Err(e) => {
                    warn!(log, "Failed to read graffiti file"; "error" => ?e);
                    None
                }
            })
            .or(self.validator_store.graffiti(&validator_pubkey).await)
            .or(self.graffiti);

        let randao_reveal_ref = &randao_reveal;
        let self_ref = &self;
        let proposer_index = self.validator_store.validator_index(&validator_pubkey).await;
        let validator_pubkey_ref = &validator_pubkey;
        let signed_block = self
            .beacon_nodes
            .first_success(RequireSynced::No, OfflineOnFailure::Yes, |beacon_node| async move {
                let get_timer = metrics::start_timer_vec(
                    &metrics::BLOCK_SERVICE_TIMES,
                    &[metrics::BEACON_BLOCK_HTTP_GET],
                );
                let block = match Payload::block_type() {
                    BlockType::Full => {
                        beacon_node
                            .get_validator_blocks::<E, Payload>(
                                slot,
                                randao_reveal_ref,
                                graffiti.as_ref(),
                            )
                            .await
                            .map_err(|e| {
                                BlockError::Recoverable(format!(
                                    "Error from beacon node when producing block: {:?}",
                                    e
                                ))
                            })?
                            .data
                    }
                    BlockType::Blinded => {
                        beacon_node
                            .get_validator_blinded_blocks::<E, Payload>(
                                slot,
                                randao_reveal_ref,
                                graffiti.as_ref(),
                            )
                            .await
                            .map_err(|e| {
                                BlockError::Recoverable(format!(
                                    "Error from beacon node when producing block: {:?}",
                                    e
                                ))
                            })?
                            .data
                    }
                };
                drop(get_timer);

                if proposer_index != Some(block.proposer_index()) {
                    return Err(BlockError::Recoverable(
                        "Proposer index does not match block proposer. Beacon chain re-orged"
                            .to_string(),
                    ));
                }

                let signed_block = self_ref
                    .validator_store
                    .sign_block::<Payload>(*validator_pubkey_ref, block, current_slot)
                    .await
                    .map_err(|e| {
                        match e {
                            VSError::UnableToSign(SigningError::NotLeader) => BlockError::SignBlockNotLeader,
                            _ => BlockError::Recoverable(format!("Unable to sign block: {:?}", e))
                        }
                    })?;

                let _post_timer = metrics::start_timer_vec(
                    &metrics::BLOCK_SERVICE_TIMES,
                    &[metrics::BEACON_BLOCK_HTTP_POST],
                );

                match Payload::block_type() {
                    BlockType::Full => beacon_node
                        .post_beacon_blocks(&signed_block)
                        .await
                        .map_err(|e| {
                            BlockError::Irrecoverable(format!(
                                "Error from beacon node when publishing block: {:?}",
                                e
                            ))
                        })?,
                    BlockType::Blinded => beacon_node
                        .post_beacon_blinded_blocks(&signed_block)
                        .await
                        .map_err(|e| {
                            BlockError::Irrecoverable(format!(
                                "Error from beacon node when publishing block: {:?}",
                                e
                            ))
                        })?,
                }

                Ok::<_, BlockError>(signed_block)
            })
            .await?;

        info!(
            log,
            "Successfully published block";
            "deposits" => signed_block.message().body().deposits().len(),
            "attestations" => signed_block.message().body().attestations().len(),
            "graffiti" => ?graffiti.map(|g| g.as_utf8_lossy()),
            "slot" => signed_block.slot().as_u64(),
        );

        Ok(())
    }
}

