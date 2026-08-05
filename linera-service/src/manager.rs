// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The validator manager: monitors per-worker load and adapts the
//! chain-to-worker assignment (and, optionally, the set of running workers) to
//! the current demand.
//!
//! The policy is deliberately standard: threshold-based greedy rebalancing
//! with hysteresis. Every polling round the manager collects per-chain request
//! counters from all workers and then:
//!
//! 1. *Scales up* when some worker is above the high watermark and no active
//!    worker has spare capacity: it starts the worker for the next inactive
//!    elastic shard slot (see `base_shards` in the network configuration).
//! 2. *Rebalances* by greedily migrating the hottest chains from overloaded
//!    workers to the least-loaded worker, but only when doing so strictly
//!    reduces the imbalance — this rules out ping-ponging a chain between two
//!    workers.
//! 3. *Scales down* elastic workers that it started itself once they have been
//!    below the low watermark for several consecutive rounds and the remaining
//!    workers have capacity to absorb their load: their chains are migrated
//!    back to the chains' default shards and the process is stopped.
//!
//! A cooldown after every scale event prevents thrashing. Migrations use the
//! drain-based handover protocol (see `linera_rpc::routing`), so rebalancing a
//! chain does not interrupt other chains.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::Duration,
};

use anyhow::Context as _;
use linera_base::identifiers::ChainId;
use linera_rpc::{
    config::{NetworkProtocol, ShardId, ValidatorInternalNetworkConfig},
    grpc::{
        api::{
            self, notifier_service_client::NotifierServiceClient,
            validator_worker_client::ValidatorWorkerClient,
        },
        GRPC_MAX_MESSAGE_SIZE,
    },
};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;
use tracing::{error, info, warn};

/// A worker whose load stays below `HIGH * SPARE_FRACTION` is considered to
/// have spare capacity, so no new worker needs to be started.
const SPARE_FRACTION: f64 = 0.5;
/// When draining a worker, the busiest remaining worker must stay below
/// `HIGH * ABSORB_FRACTION` after absorbing the drained load.
const ABSORB_FRACTION: f64 = 0.8;
/// How long to wait for a freshly spawned worker to answer its first RPC.
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-request timeout when collecting load reports.
const REPORT_TIMEOUT: Duration = Duration::from_secs(2);

/// Configuration of the validator manager.
#[derive(Clone, Debug)]
pub struct ManagerConfig {
    /// How often load is collected and the policy is evaluated.
    pub interval: Duration,
    /// Per-worker request rate (requests/sec) above which a worker is
    /// considered overloaded.
    pub high_watermark: f64,
    /// Per-worker request rate below which an elastic worker is considered
    /// idle and eligible for scale-down.
    pub low_watermark: f64,
    /// Maximum number of chain migrations executed per round.
    pub max_migrations_per_round: usize,
    /// Number of consecutive idle rounds before an elastic worker is drained.
    pub scale_down_rounds: usize,
    /// Number of rounds without scale actions after a scale event.
    pub cooldown_rounds: usize,
    /// Command template used to start the worker for an elastic shard slot;
    /// every occurrence of `{shard}` is replaced by the shard number. When
    /// absent, the manager only rebalances across already-running workers.
    pub spawn_command: Option<Vec<String>>,
}

/// The load observed on one worker during the last round.
#[derive(Clone, Debug, Default)]
pub struct WorkerLoad {
    /// Requests per second, per chain served by this worker.
    pub chain_rates: BTreeMap<ChainId, f64>,
}

impl WorkerLoad {
    /// Returns the total request rate of the worker.
    pub fn total(&self) -> f64 {
        self.chain_rates.values().sum()
    }
}

/// One planned management action.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Migrate a chain between two active workers.
    Migrate {
        /// The chain to migrate.
        chain_id: ChainId,
        /// The worker currently owning the chain.
        from: ShardId,
        /// The worker that should take the chain over.
        to: ShardId,
    },
    /// Start the worker for an inactive elastic shard slot.
    SpawnWorker {
        /// The elastic shard slot to activate.
        shard: ShardId,
    },
    /// Migrate all chains off an idle elastic worker, then stop it.
    DrainWorker {
        /// The elastic worker to drain and stop.
        shard: ShardId,
    },
}

/// Plans management actions from the current per-worker load.
///
/// `loads` contains the workers that answered the last load poll. Slots in
/// `inactive_elastic` may be activated by a `SpawnWorker` action; workers in
/// `killable` may be shut down by a `DrainWorker` action once they have been
/// idle for `config.scale_down_rounds` rounds (per `quiet_rounds`). The caller
/// enforces cooldowns by passing empty `inactive_elastic`/`killable` sets.
pub fn plan(
    loads: &BTreeMap<ShardId, WorkerLoad>,
    inactive_elastic: &[ShardId],
    killable: &BTreeSet<ShardId>,
    quiet_rounds: &HashMap<ShardId, usize>,
    config: &ManagerConfig,
) -> Vec<Action> {
    let mut actions = Vec::new();
    // Simulated per-worker rates, updated as migrations are planned.
    let mut rates: BTreeMap<ShardId, f64> = loads
        .iter()
        .map(|(shard, load)| (*shard, load.total()))
        .collect();
    // Chains already planned for migration this round; a chain moves at most
    // once per round.
    let mut moved_chains = BTreeSet::new();
    // Workers receiving migrations this round must not be drained this round.
    let mut migration_targets = BTreeSet::new();

    // Step 1: scale up if someone is overloaded and nobody has spare capacity.
    let overloaded = rates.values().any(|rate| *rate > config.high_watermark);
    let has_spare = rates
        .values()
        .any(|rate| *rate < config.high_watermark * SPARE_FRACTION);
    if overloaded && !has_spare {
        if let Some(shard) = inactive_elastic.first() {
            actions.push(Action::SpawnWorker { shard: *shard });
            // Plan migrations onto the new worker right away.
            rates.insert(*shard, 0.0);
        }
    }

    // Step 2: greedy rebalancing, hottest chain first.
    for _ in 0..config.max_migrations_per_round {
        if rates.len() < 2 {
            break;
        }
        let (&source, &source_rate) = rates
            .iter()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .expect("rates is non-empty");
        if source_rate <= config.high_watermark {
            break;
        }
        let (&target, &target_rate) = rates
            .iter()
            .filter(|(shard, _)| **shard != source)
            .min_by(|a, b| a.1.total_cmp(b.1))
            .expect("rates has at least two entries");
        // Pick the hottest chain whose move strictly reduces the imbalance.
        // (This also rejects moves that would just swap the roles of the two
        // workers, preventing ping-pong migrations.)
        let candidate = loads.get(&source).and_then(|load| {
            load.chain_rates
                .iter()
                .filter(|(chain_id, rate)| {
                    !moved_chains.contains(*chain_id)
                        && **rate > 0.0
                        && target_rate + **rate < source_rate - **rate
                })
                .max_by(|a, b| a.1.total_cmp(b.1))
        });
        let Some((&chain_id, &chain_rate)) = candidate else {
            break;
        };
        actions.push(Action::Migrate {
            chain_id,
            from: source,
            to: target,
        });
        moved_chains.insert(chain_id);
        migration_targets.insert(target);
        *rates.get_mut(&source).expect("source is present") -= chain_rate;
        *rates.get_mut(&target).expect("target is present") += chain_rate;
    }

    // Step 3: scale down elastic workers that have been idle long enough.
    for shard in killable {
        if migration_targets.contains(shard) {
            continue;
        }
        let Some(&rate) = rates.get(shard) else {
            continue;
        };
        if rate > config.low_watermark {
            continue;
        }
        if quiet_rounds.get(shard).copied().unwrap_or(0) < config.scale_down_rounds {
            continue;
        }
        // The busiest remaining worker must have room to absorb the drained
        // load even in the worst case (all of it landing on one worker).
        let max_other = rates
            .iter()
            .filter(|(other, _)| *other != shard)
            .map(|(_, rate)| *rate)
            .fold(f64::NEG_INFINITY, f64::max);
        if max_other + rate > config.high_watermark * ABSORB_FRACTION {
            continue;
        }
        actions.push(Action::DrainWorker { shard: *shard });
        rates.remove(shard);
    }

    actions
}

/// Orchestrates the runtime migration of a chain between two shards (workers)
/// of the same validator.
///
/// The protocol is: (1) the source worker drains in-flight work for the chain
/// and releases ownership, (2) the target worker takes ownership and persists
/// the new assignment to the validator's shared storage, (3) the remaining
/// workers and the proxies are notified of the routing change. Workers forward
/// misrouted requests to the current owner, so a failed notification degrades
/// performance for the affected chain but not correctness. Migrations are
/// expected to be issued one at a time.
pub async fn migrate_chain(
    network: &ValidatorInternalNetworkConfig,
    chain_id: ChainId,
    to_shard: usize,
    from_shard: Option<usize>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(network.protocol, NetworkProtocol::Grpc(_)),
        "chain migration requires the gRPC internal protocol"
    );
    anyhow::ensure!(
        to_shard < network.shards.len(),
        "shard {to_shard} does not exist; validator has {} shards",
        network.shards.len()
    );

    // Resolve the source shard from the target worker's routing table if it was
    // not provided explicitly.
    let from_shard = match from_shard {
        Some(shard) => shard,
        None => {
            let mut client = worker_client(network, to_shard).await?;
            let assignment = client
                .get_shard_assignment(api::ChainId::from(chain_id))
                .await
                .context("failed to query current shard assignment")?
                .into_inner();
            usize::try_from(assignment.shard_id)
                .context("target worker reported an invalid shard assignment")?
        }
    };
    anyhow::ensure!(
        from_shard != to_shard,
        "chain {chain_id} is already assigned to shard {to_shard}"
    );
    anyhow::ensure!(
        from_shard < network.shards.len(),
        "shard {from_shard} does not exist; validator has {} shards",
        network.shards.len()
    );
    info!("Migrating chain {chain_id} from shard {from_shard} to shard {to_shard}");

    let update = api::ShardAssignmentUpdate {
        chain_id: Some(chain_id.into()),
        shard_id: to_shard as u64,
    };

    // Step 1: the source worker drains and releases the chain.
    let mut source = worker_client(network, from_shard).await?;
    source
        .release_chain(update.clone())
        .await
        .context("source worker failed to release the chain")?;
    info!("Shard {from_shard} released chain {chain_id}");

    // Step 2: the target worker takes ownership and persists the assignment.
    let mut target = worker_client(network, to_shard).await?;
    target
        .acquire_chain(update.clone())
        .await
        .context("target worker failed to acquire the chain")?;
    info!("Shard {to_shard} acquired chain {chain_id}");

    // Step 3: notify the remaining workers, for cross-chain request routing.
    for shard in 0..network.shards.len() {
        if shard == from_shard || shard == to_shard {
            continue;
        }
        let result = async {
            let mut client = worker_client(network, shard).await?;
            client.update_shard_assignment(update.clone()).await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = result {
            warn!(
                "Failed to notify shard {shard} of the new assignment: {error}. \
                 Its requests for chain {chain_id} will be forwarded by shard {from_shard}."
            );
        }
    }

    // Step 4: notify the proxies, for client request routing.
    for (id, proxy) in network.proxies.iter().enumerate() {
        let address = proxy.internal_address(&network.protocol);
        let result = async {
            let mut client = NotifierServiceClient::connect(address.clone())
                .await
                .with_context(|| format!("failed to connect to proxy {id} at {address}"))?;
            client.update_shard_assignment(update.clone()).await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = result {
            error!(
                "Failed to notify proxy {id} of the new assignment: {error}. \
                 Its requests for chain {chain_id} will be forwarded by shard {from_shard}."
            );
        }
    }

    info!("Chain {chain_id} migrated to shard {to_shard}");
    Ok(())
}

async fn worker_client(
    network: &ValidatorInternalNetworkConfig,
    shard: ShardId,
) -> anyhow::Result<ValidatorWorkerClient<Channel>> {
    let address = network.shard(shard).http_address();
    let client = ValidatorWorkerClient::connect(address.clone())
        .await
        .with_context(|| format!("failed to connect to worker for shard {shard} at {address}"))?;
    Ok(client
        .max_encoding_message_size(GRPC_MAX_MESSAGE_SIZE)
        .max_decoding_message_size(GRPC_MAX_MESSAGE_SIZE))
}

/// The long-running validator manager.
pub struct Manager {
    network: ValidatorInternalNetworkConfig,
    config: ManagerConfig,
    /// Elastic worker processes started (and owned) by this manager.
    children: HashMap<ShardId, Child>,
    /// Consecutive rounds each elastic worker has spent below the low
    /// watermark.
    quiet_rounds: HashMap<ShardId, usize>,
    /// Remaining rounds during which no scale actions are taken.
    cooldown: usize,
}

impl Manager {
    /// Creates a new manager for the given validator network.
    pub fn new(
        network: ValidatorInternalNetworkConfig,
        config: ManagerConfig,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            matches!(network.protocol, NetworkProtocol::Grpc(_)),
            "the validator manager requires the gRPC internal protocol"
        );
        anyhow::ensure!(
            config.high_watermark > 0.0 && config.low_watermark < config.high_watermark,
            "watermarks must satisfy 0 < low < high"
        );
        if config.spawn_command.is_some() {
            anyhow::ensure!(
                network.num_base_shards() < network.shards.len(),
                "the configuration has no elastic shard slots (base_shards must be smaller \
                 than the number of shards to allow spawning workers)"
            );
        }
        Ok(Self {
            network,
            config,
            children: HashMap::new(),
            quiet_rounds: HashMap::new(),
            cooldown: 0,
        })
    }

    /// Runs the manager until the shutdown token is cancelled. On shutdown,
    /// all elastic workers started by this manager are drained and stopped.
    pub async fn run(mut self, shutdown: CancellationToken) -> anyhow::Result<()> {
        info!(
            base_shards = self.network.num_base_shards(),
            total_shards = self.network.shards.len(),
            interval = ?self.config.interval,
            high_watermark = self.config.high_watermark,
            low_watermark = self.config.low_watermark,
            spawning = self.config.spawn_command.is_some(),
            "starting validator manager"
        );
        let mut interval = tokio::time::interval(self.config.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; skip it so that the first load
        // report covers a full interval.
        interval.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(error) = self.round().await {
                        warn!(%error, "management round failed");
                    }
                }
            }
        }
        self.shutdown_drain().await;
        Ok(())
    }

    /// Executes one management round: collect load, plan, act.
    async fn round(&mut self) -> anyhow::Result<()> {
        self.reap_children();
        let loads = self.collect_loads().await;
        if loads.is_empty() {
            warn!("no worker answered the load poll; skipping round");
            return Ok(());
        }

        // Track idle rounds for elastic workers.
        let num_base = self.network.num_base_shards();
        for (shard, load) in &loads {
            if *shard >= num_base {
                if load.total() <= self.config.low_watermark {
                    *self.quiet_rounds.entry(*shard).or_insert(0) += 1;
                } else {
                    self.quiet_rounds.insert(*shard, 0);
                }
            }
        }

        // During a cooldown only rebalancing is allowed, no scale actions.
        let (inactive_elastic, killable) = if self.cooldown > 0 {
            self.cooldown -= 1;
            (Vec::new(), BTreeSet::new())
        } else {
            let inactive_elastic: Vec<ShardId> = if self.config.spawn_command.is_some() {
                (num_base..self.network.shards.len())
                    .filter(|shard| !loads.contains_key(shard))
                    .collect()
            } else {
                Vec::new()
            };
            let killable: BTreeSet<ShardId> = self
                .children
                .keys()
                .filter(|shard| loads.contains_key(shard))
                .copied()
                .collect();
            (inactive_elastic, killable)
        };

        let actions = plan(
            &loads,
            &inactive_elastic,
            &killable,
            &self.quiet_rounds,
            &self.config,
        );
        for action in actions {
            if let Err(error) = self.execute(action.clone()).await {
                warn!(?action, %error, "management action failed");
            }
        }
        Ok(())
    }

    /// Collects load reports from all configured shards. Shards that do not
    /// answer within the timeout are treated as inactive.
    async fn collect_loads(&self) -> BTreeMap<ShardId, WorkerLoad> {
        let interval_secs = self.config.interval.as_secs_f64();
        let reports = futures::future::join_all((0..self.network.shards.len()).map(|shard| {
            let network = &self.network;
            async move {
                let report = tokio::time::timeout(REPORT_TIMEOUT, async {
                    let mut client = worker_client(network, shard).await?;
                    anyhow::Ok(client.get_load_report(()).await?.into_inner())
                })
                .await;
                (shard, report)
            }
        }))
        .await;

        let mut loads = BTreeMap::new();
        for (shard, report) in reports {
            match report {
                Ok(Ok(report)) => {
                    let mut chain_rates = BTreeMap::new();
                    for load in report.loads {
                        let Some(chain_id) = load.chain_id else {
                            continue;
                        };
                        let Ok(chain_id) = ChainId::try_from(chain_id) else {
                            continue;
                        };
                        #[expect(
                            clippy::cast_precision_loss,
                            reason = "request counts are far below 2^52"
                        )]
                        chain_rates.insert(chain_id, load.requests as f64 / interval_secs);
                    }
                    loads.insert(shard, WorkerLoad { chain_rates });
                }
                Ok(Err(_)) | Err(_) => {
                    // Not running (an inactive elastic slot) or unreachable.
                }
            }
        }
        loads
    }

    /// Executes a single planned action.
    async fn execute(&mut self, action: Action) -> anyhow::Result<()> {
        match action {
            Action::Migrate { chain_id, from, to } => {
                info!(%chain_id, from, to, "rebalancing chain");
                migrate_chain(&self.network, chain_id, to, Some(from)).await
            }
            Action::SpawnWorker { shard } => {
                self.spawn_worker(shard)?;
                self.wait_until_ready(shard).await?;
                info!(shard, "elastic worker is ready");
                self.quiet_rounds.insert(shard, 0);
                self.cooldown = self.config.cooldown_rounds;
                Ok(())
            }
            Action::DrainWorker { shard } => {
                self.drain_worker(shard).await?;
                self.cooldown = self.config.cooldown_rounds;
                Ok(())
            }
        }
    }

    /// Starts the worker process for an elastic shard slot.
    fn spawn_worker(&mut self, shard: ShardId) -> anyhow::Result<()> {
        let template = self
            .config
            .spawn_command
            .as_ref()
            .context("no spawn command configured")?;
        let args: Vec<String> = template
            .iter()
            .map(|token| token.replace("{shard}", &shard.to_string()))
            .collect();
        anyhow::ensure!(!args.is_empty(), "empty spawn command");
        info!(shard, command = ?args, "starting elastic worker");
        let mut command = Command::new(&args[0]);
        command.args(&args[1..]);
        // If the manager dies unexpectedly, its workers die with it; a
        // restarted manager will spawn them again on demand. Chains assigned
        // to a dead worker are unreachable until then, so run the manager
        // under supervision in production settings.
        command.kill_on_drop(true);
        let child = command
            .spawn()
            .with_context(|| format!("failed to start worker for shard {shard}"))?;
        self.children.insert(shard, child);
        Ok(())
    }

    /// Waits until a freshly spawned worker answers its first load report.
    async fn wait_until_ready(&self, shard: ShardId) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + SPAWN_READY_TIMEOUT;
        loop {
            let ready = tokio::time::timeout(REPORT_TIMEOUT, async {
                let mut client = worker_client(&self.network, shard).await?;
                anyhow::Ok(client.get_load_report(()).await?)
            })
            .await;
            if matches!(ready, Ok(Ok(_))) {
                return Ok(());
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "worker for shard {shard} did not become ready in time"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Migrates all chains off an elastic worker (back to their default
    /// shards, which clears their routing overrides), then stops its process.
    async fn drain_worker(&mut self, shard: ShardId) -> anyhow::Result<()> {
        info!(shard, "draining elastic worker");
        let mut client = worker_client(&self.network, shard).await?;
        let assignments = client
            .list_shard_assignments(())
            .await
            .context("failed to list the worker's chain assignments")?
            .into_inner();
        for entry in assignments.entries {
            if usize::try_from(entry.shard_id) != Ok(shard) {
                continue;
            }
            let Some(chain_id) = entry.chain_id else {
                continue;
            };
            let Ok(chain_id) = ChainId::try_from(chain_id) else {
                continue;
            };
            let default_shard = self.network.get_shard_id(chain_id);
            if let Err(error) =
                migrate_chain(&self.network, chain_id, default_shard, Some(shard)).await
            {
                warn!(%chain_id, %error, "failed to migrate chain off drained worker");
            }
        }
        if let Some(mut child) = self.children.remove(&shard) {
            if let Err(error) = child.kill().await {
                warn!(shard, %error, "failed to stop elastic worker process");
            } else {
                info!(shard, "elastic worker stopped");
            }
        }
        self.quiet_rounds.remove(&shard);
        Ok(())
    }

    /// Removes children whose processes have already exited.
    fn reap_children(&mut self) {
        self.children.retain(|shard, child| match child.try_wait() {
            Ok(Some(status)) => {
                warn!(shard, %status, "elastic worker exited unexpectedly");
                false
            }
            Ok(None) => true,
            Err(error) => {
                warn!(shard, %error, "failed to poll elastic worker process");
                true
            }
        });
    }

    /// Drains and stops all elastic workers started by this manager.
    async fn shutdown_drain(&mut self) {
        let shards: Vec<ShardId> = self.children.keys().copied().collect();
        if shards.is_empty() {
            return;
        }
        info!(?shards, "shutting down: draining elastic workers");
        for shard in shards {
            if let Err(error) = self.drain_worker(shard).await {
                warn!(shard, %error, "failed to drain elastic worker during shutdown");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use linera_base::crypto::CryptoHash;

    use super::*;

    fn chain(index: u8) -> ChainId {
        ChainId(CryptoHash::test_hash(format!("chain{index}")))
    }

    fn config() -> ManagerConfig {
        ManagerConfig {
            interval: Duration::from_secs(1),
            high_watermark: 100.0,
            low_watermark: 25.0,
            max_migrations_per_round: 8,
            scale_down_rounds: 3,
            cooldown_rounds: 2,
            spawn_command: Some(vec!["true".to_string()]),
        }
    }

    fn load(rates: &[(ChainId, f64)]) -> WorkerLoad {
        WorkerLoad {
            chain_rates: rates.iter().copied().collect(),
        }
    }

    #[test]
    fn test_no_actions_when_balanced() {
        let loads = BTreeMap::from([
            (0, load(&[(chain(0), 50.0)])),
            (1, load(&[(chain(1), 60.0)])),
        ]);
        let actions = plan(&loads, &[2], &BTreeSet::new(), &HashMap::new(), &config());
        assert_eq!(actions, Vec::new());
    }

    #[test]
    fn test_rebalances_hot_chain_to_least_loaded_worker() {
        let loads = BTreeMap::from([
            (
                0,
                load(&[(chain(0), 90.0), (chain(1), 40.0), (chain(2), 10.0)]),
            ),
            (1, load(&[(chain(3), 10.0)])),
        ]);
        let actions = plan(&loads, &[], &BTreeSet::new(), &HashMap::new(), &config());
        // The hottest chain that strictly reduces the imbalance is moved:
        // moving chain 0 (90) would leave 50 vs 100 — allowed (100 < 140-?)...
        // moving it gives source 50, target 100: 10 + 90 = 100 < 140 - 90 = 50
        // is false, so chain 1 (40) is chosen: 10 + 40 = 50 < 140 - 40 = 100.
        assert_eq!(
            actions,
            vec![Action::Migrate {
                chain_id: chain(1),
                from: 0,
                to: 1,
            }]
        );
    }

    #[test]
    fn test_spawns_worker_when_no_spare_capacity() {
        let loads = BTreeMap::from([
            (0, load(&[(chain(0), 80.0), (chain(1), 70.0)])),
            (1, load(&[(chain(2), 90.0)])),
        ]);
        let actions = plan(
            &loads,
            &[2, 3],
            &BTreeSet::new(),
            &HashMap::new(),
            &config(),
        );
        assert_eq!(actions[0], Action::SpawnWorker { shard: 2 });
        // And chains are rebalanced onto the new worker in the same round.
        assert!(actions
            .iter()
            .any(|action| matches!(action, Action::Migrate { to: 2, .. })));
    }

    #[test]
    fn test_does_not_spawn_during_light_load() {
        let loads = BTreeMap::from([
            (0, load(&[(chain(0), 30.0)])),
            (1, load(&[(chain(1), 20.0)])),
        ]);
        let actions = plan(&loads, &[2], &BTreeSet::new(), &HashMap::new(), &config());
        assert_eq!(actions, Vec::new());
    }

    #[test]
    fn test_drains_idle_elastic_worker_after_quiet_period() {
        let loads = BTreeMap::from([
            (0, load(&[(chain(0), 30.0)])),
            (2, load(&[(chain(1), 5.0)])),
        ]);
        let killable = BTreeSet::from([2]);
        // Not yet quiet for long enough.
        let quiet = HashMap::from([(2, 1)]);
        assert_eq!(plan(&loads, &[], &killable, &quiet, &config()), Vec::new());
        // Quiet for long enough.
        let quiet = HashMap::from([(2, 3)]);
        assert_eq!(
            plan(&loads, &[], &killable, &quiet, &config()),
            vec![Action::DrainWorker { shard: 2 }]
        );
    }

    #[test]
    fn test_does_not_drain_when_remaining_workers_are_busy() {
        let loads = BTreeMap::from([
            (0, load(&[(chain(0), 79.0)])),
            (2, load(&[(chain(1), 5.0)])),
        ]);
        let killable = BTreeSet::from([2]);
        let quiet = HashMap::from([(2, 10)]);
        // 79 + 5 > 100 * 0.8, so worker 2 must stay.
        assert_eq!(plan(&loads, &[], &killable, &quiet, &config()), Vec::new());
    }

    #[test]
    fn test_no_ping_pong_for_single_hot_chain() {
        // One chain produces all the load; moving it cannot reduce the
        // imbalance, so nothing should happen — in this round or the next.
        let loads = BTreeMap::from([(0, load(&[(chain(0), 150.0)])), (1, load(&[]))]);
        let actions = plan(&loads, &[], &BTreeSet::new(), &HashMap::new(), &config());
        assert_eq!(actions, Vec::new());
    }

    #[test]
    fn test_migration_limit_per_round() {
        let chains: Vec<(ChainId, f64)> = (0..32).map(|i| (chain(i), 20.0)).collect();
        let loads = BTreeMap::from([(0, load(&chains)), (1, load(&[]))]);
        let mut config = config();
        config.max_migrations_per_round = 4;
        let actions = plan(&loads, &[], &BTreeSet::new(), &HashMap::new(), &config);
        assert_eq!(actions.len(), 4);
    }
}
