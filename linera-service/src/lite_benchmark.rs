// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A lightweight benchmark client that spams transactions through real consensus without
//! running a full node.
//!
//! Unlike `linera-benchmark` (which drives every transaction through a `ChainClient`, and
//! therefore needs a full `Storage` backend and locally executes every block's WASM
//! application before submitting it), this client only does the minimum work required to
//! produce a validly-signed block proposal: it tracks the tip hash/height/epoch of each
//! chain itself, signs a fresh `BlockProposal` with no execution outcome attached, and lets
//! the validators compute and vote on the result. It does not verify validator signatures
//! (it trusts responses at face value and simply counts successes/failures) but it does
//! drive the real propose -> vote -> certificate -> commit cycle, so the validators see and
//! process the same consensus traffic they would from any other client.
//!
//! Every benchmarked chain must be owned by a single *super owner* (see
//! `linera open-chain --super-owner`), so that every block proposal is made in `Round::Fast`
//! and validators vote to confirm it directly, without the validate-then-confirm two-phase
//! exchange that other rounds require.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context as _, Result};
use clap::Parser;
use futures::future::join_all;
use linera_base::{
    crypto::{CryptoHash, InMemorySigner, Signer as _, ValidatorPublicKey, ValidatorSignature},
    data_types::{Epoch, Round, Timestamp},
    identifiers::{AccountOwner, ChainId},
};
use linera_cache::ValueCache;
use linera_chain::{
    data_types::{BlockProposal, ProposedBlock, Transaction},
    justification::JustificationChain,
    types::{
        CertificateKind, CertificateValue as _, ConfirmedBlock, ConfirmedBlockCertificate,
        GenericCertificate,
    },
};
use linera_client::benchmark::{NativeFungibleTransferGenerator, OperationGenerator};
use linera_core::{
    data_types::ChainInfoQuery,
    node::{CrossChainMessageDelivery, ValidatorNode, ValidatorNodeProvider as _},
};
use linera_execution::committee::Committee;
use linera_rpc::{node_provider::DEFAULT_MAX_BACKOFF, Client, NodeOptions, NodeProvider};
use linera_wallet_json::{Keystore, PersistentWallet};
use num_format::{Locale, ToFormattedString as _};
use tokio::{task, time};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(clap::Parser)]
#[command(
    name = "linera-lite-benchmark",
    version = linera_version::VersionInfo::default_clap_str(),
    about = "Spam transactions through consensus with a minimal, storage-free client",
)]
struct Args {
    /// Path to the wallet file (for the genesis config / committee and chain ownership).
    #[arg(long)]
    wallet: PathBuf,

    /// Path to the keystore file (for the signing keys of the chains' super owners).
    #[arg(long)]
    keystore: PathBuf,

    /// The chains to benchmark. Each must be owned by a single super owner whose key is in
    /// the keystore. Defaults to every chain in the wallet that has an owner.
    #[arg(long, value_delimiter = ',')]
    chains: Vec<ChainId>,

    /// Target number of blocks per second, summed across all benchmarked chains.
    #[arg(long, default_value = "1")]
    bps: usize,

    /// Number of operations to include in each block.
    #[arg(long, default_value = "1")]
    transactions_per_block: usize,

    /// If set, stop after this many seconds.
    #[arg(long)]
    runtime_in_seconds: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    linera_service::tracing::init("lite-benchmark");
    let args = Args::parse();

    let wallet = PersistentWallet::read(&args.wallet).context("failed to read the wallet")?;
    let keystore = Keystore::read(&args.keystore).context("failed to read the keystore")?;
    let signer = keystore.into_signer();

    let committee = wallet.genesis_config().committee.clone();
    let node_provider = NodeProvider::new(NodeOptions {
        send_timeout: Duration::from_secs(4),
        recv_timeout: Duration::from_secs(4),
        retry_delay: Duration::from_millis(200),
        max_retries: 10,
        max_backoff: DEFAULT_MAX_BACKOFF,
    });
    let nodes: Vec<(ValidatorPublicKey, Client)> = node_provider
        .make_nodes(&committee)
        .context("failed to create validator node clients")?
        .collect();
    anyhow::ensure!(!nodes.is_empty(), "the committee has no validators");

    let chain_ids = if args.chains.is_empty() {
        wallet.owned_chain_ids()
    } else {
        args.chains.clone()
    };
    anyhow::ensure!(!chain_ids.is_empty(), "no chains to benchmark");

    let mut chain_clients = Vec::new();
    for chain_id in chain_ids {
        let owner = wallet
            .get(chain_id)
            .and_then(|chain| chain.owner)
            .with_context(|| format!("chain {chain_id} has no owner in the wallet"))?;
        anyhow::ensure!(
            signer.contains_key(&owner).await.unwrap_or(false),
            "the keystore has no key for owner {owner} of chain {chain_id}"
        );
        let client = LiteChainClient::seed(
            chain_id,
            owner,
            nodes.clone(),
            committee.clone(),
            signer.clone(),
        )
        .await
        .with_context(|| format!("failed to seed the initial state for chain {chain_id}"))?;
        chain_clients.push(client);
    }

    let shutdown = CancellationToken::new();
    if let Some(runtime_in_seconds) = args.runtime_in_seconds {
        let shutdown = shutdown.clone();
        task::spawn(async move {
            time::sleep(Duration::from_secs(runtime_in_seconds)).await;
            shutdown.cancel();
        });
    }

    let num_chains = chain_clients.len();
    let bps_initial_share = args.bps / num_chains;
    let mut bps_remainder = args.bps % num_chains;
    let success_count = Arc::new(AtomicUsize::new(0));
    let failure_count = Arc::new(AtomicUsize::new(0));

    let mut join_set = task::JoinSet::new();
    for client in chain_clients {
        let bps_share = if bps_remainder > 0 {
            bps_remainder -= 1;
            bps_initial_share + 1
        } else {
            bps_initial_share
        };
        let shutdown = shutdown.clone();
        let success_count = success_count.clone();
        let failure_count = failure_count.clone();
        let owner = client.owner;
        let chain_id = client.chain_id;
        let generator = NativeFungibleTransferGenerator::new(chain_id, vec![], true)
            .map_err(|error| anyhow!("failed to create the operation generator: {error}"))?;
        let transactions_per_block = args.transactions_per_block;
        join_set.spawn(run_chain(
            client,
            generator,
            owner,
            bps_share,
            transactions_per_block,
            shutdown,
            success_count,
            failure_count,
        ));
    }

    let report_success_count = success_count.clone();
    let report_failure_count = failure_count.clone();
    let report_shutdown = shutdown.clone();
    let report_task = task::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = report_shutdown.cancelled() => break,
                _ = interval.tick() => {
                    let successes = report_success_count.swap(0, Ordering::Relaxed);
                    let failures = report_failure_count.swap(0, Ordering::Relaxed);
                    info!(
                        "{} blocks/s committed, {} failed",
                        successes.to_formatted_string(&Locale::en),
                        failures.to_formatted_string(&Locale::en),
                    );
                }
            }
        }
    });

    while let Some(result) = join_set.join_next().await {
        result??;
    }
    shutdown.cancel();
    report_task.await?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_chain(
    mut client: LiteChainClient,
    mut generator: NativeFungibleTransferGenerator,
    owner: AccountOwner,
    bps: usize,
    transactions_per_block: usize,
    shutdown: CancellationToken,
    success_count: Arc<AtomicUsize>,
    failure_count: Arc<AtomicUsize>,
) -> Result<()> {
    let chain_id = client.chain_id;
    let mut interval = if bps > 0 {
        Some(time::interval(Duration::from_secs_f64(1.0 / bps as f64)))
    } else {
        None
    };
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        if let Some(interval) = &mut interval {
            interval.tick().await;
        }
        let operations = generator.generate_operations(owner, transactions_per_block);
        match client.propose_and_commit(operations).await {
            Ok(()) => {
                success_count.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                warn!(%chain_id, %error, "failed to commit a block");
                failure_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    info!(%chain_id, "stopping benchmark");
    Ok(())
}

/// Tracks just enough state about one chain to keep proposing valid blocks, without any
/// local storage or execution.
struct LiteChainClient {
    chain_id: ChainId,
    owner: AccountOwner,
    epoch: Epoch,
    height: linera_base::data_types::BlockHeight,
    previous_block_hash: Option<CryptoHash>,
    nodes: Vec<(ValidatorPublicKey, Client)>,
    committee: Committee,
    signer: InMemorySigner,
    value_cache: ValueCache<CryptoHash, ConfirmedBlockCertificate>,
}

impl LiteChainClient {
    /// Seeds the client's state for `chain_id` from the first validator that answers.
    async fn seed(
        chain_id: ChainId,
        owner: AccountOwner,
        nodes: Vec<(ValidatorPublicKey, Client)>,
        committee: Committee,
        signer: InMemorySigner,
    ) -> Result<Self> {
        for (public_key, node) in &nodes {
            let query = ChainInfoQuery::new(chain_id);
            match node.handle_chain_info_query(query).await {
                Ok(response) => {
                    let info = response.info;
                    return Ok(Self {
                        chain_id,
                        owner,
                        epoch: info.epoch,
                        height: info.next_block_height,
                        previous_block_hash: info.block_hash,
                        nodes,
                        committee,
                        signer,
                        value_cache: ValueCache::new("lite-benchmark", 64, 60),
                    });
                }
                Err(error) => {
                    warn!(%public_key, %error, "validator did not answer the initial chain info query");
                }
            }
        }
        bail!("no validator answered the initial chain info query");
    }

    /// Builds, signs, and submits a block with the given operations, then drives it to a
    /// committed certificate. Uses `Round::Fast`, so this only works on chains owned by a
    /// single super owner.
    async fn propose_and_commit(
        &mut self,
        operations: Vec<linera_execution::Operation>,
    ) -> Result<()> {
        let transactions = operations
            .into_iter()
            .map(Transaction::ExecuteOperation)
            .collect();
        let block = ProposedBlock {
            chain_id: self.chain_id,
            epoch: self.epoch,
            transactions,
            height: self.height,
            timestamp: Timestamp::now(),
            authenticated_owner: Some(self.owner),
            previous_block_hash: self.previous_block_hash,
        };
        let proposal = BlockProposal::new_initial(self.owner, Round::Fast, block, &self.signer)
            .await
            .map_err(|error| anyhow!("failed to sign the block proposal: {error}"))?;

        // Broadcast the proposal to every validator and collect their `ConfirmedBlock` votes.
        let responses = join_all(self.nodes.iter().map(|(public_key, node)| {
            let proposal = proposal.clone();
            let public_key = *public_key;
            let node = node.clone();
            async move { (public_key, node.handle_block_proposal(proposal).await) }
        }))
        .await;
        let votes = responses.into_iter().filter_map(|(public_key, result)| match result {
            Ok(response) => response.info.manager.pending.map(|vote| (public_key, vote)),
            Err(error) => {
                warn!(%public_key, %error, "validator rejected the block proposal");
                None
            }
        });
        let (value_hash, signatures) = find_confirming_quorum(self.chain_id, votes, &self.committee)
            .context("no quorum of validators voted to confirm the proposed block")?;

        // Fetch the confirmed value (with its real execution outcome) from any validator that
        // has seen the proposal, instead of executing the block ourselves.
        let mut confirmed_block: Option<ConfirmedBlock> = None;
        for (_, node) in &self.nodes {
            let mut query = ChainInfoQuery::new(self.chain_id);
            query.request_manager_values = true;
            if let Ok(response) = node.handle_chain_info_query(query).await {
                if let Some(value) = response.info.manager.requested_confirmed {
                    if value.hash() == value_hash {
                        confirmed_block = Some(*value);
                        break;
                    }
                }
            }
        }
        let confirmed_block =
            confirmed_block.context("could not fetch the confirmed block value")?;

        // The vote's `first_round` attestation must be reproduced exactly, since it is part of
        // what every signature covers (see `Vote::new_with_first_round`); a single super owner's
        // `Round::Fast` is always the chain's designated first round, so this is always `true`.
        let quorum = GenericCertificate::new_with_payload(
            confirmed_block,
            Round::Fast,
            None,
            true,
            None,
            signatures,
        );
        let certificate = ConfirmedBlockCertificate::from_parts(quorum, JustificationChain::default());
        let cached_certificate = self
            .value_cache
            .insert(&certificate.hash(), certificate.clone());

        // Broadcast the certificate so every validator commits the block. Only advance our own
        // state once at least one validator actually accepted it, so we don't get out of sync
        // with the chain if the certificate is rejected everywhere.
        let results = join_all(self.nodes.iter().map(|(_, node)| {
            let node = node.clone();
            let cached_certificate = cached_certificate.clone();
            async move {
                node.handle_confirmed_certificate(
                    cached_certificate,
                    CrossChainMessageDelivery::NonBlocking,
                )
                .await
            }
        }))
        .await;
        let mut committed = false;
        for result in results {
            if let Err(error) = result {
                warn!(%error, "validator failed to process the confirmed certificate");
            } else {
                committed = true;
            }
        }
        anyhow::ensure!(committed, "no validator accepted the confirmed certificate");

        self.previous_block_hash = Some(certificate.hash());
        self.height = self.height.try_add_one()?;
        Ok(())
    }
}

/// Groups the given validator votes by the `ConfirmedBlock` value hash they attest to, and
/// returns the first hash (and its signatures) whose combined committee weight reaches the
/// quorum threshold. Votes for the wrong chain or of the wrong kind are ignored. No signature
/// is verified here: the caller trusts every vote at face value.
fn find_confirming_quorum(
    chain_id: ChainId,
    votes: impl IntoIterator<Item = (ValidatorPublicKey, linera_chain::data_types::LiteVote)>,
    committee: &Committee,
) -> Option<(CryptoHash, Vec<(ValidatorPublicKey, ValidatorSignature)>)> {
    let mut signatures_by_hash: HashMap<CryptoHash, Vec<(ValidatorPublicKey, ValidatorSignature)>> =
        HashMap::new();
    let mut weight_by_hash: HashMap<CryptoHash, u64> = HashMap::new();
    for (public_key, vote) in votes {
        if vote.value.chain_id != chain_id || vote.value.kind != CertificateKind::Confirmed {
            continue;
        }
        let hash = vote.value.value_hash;
        signatures_by_hash
            .entry(hash)
            .or_default()
            .push((public_key, vote.signature));
        let weight = weight_by_hash.entry(hash).or_insert(0);
        *weight += committee.weight(&public_key);
        if *weight >= committee.quorum_threshold() {
            let signatures = signatures_by_hash.remove(&hash).expect("just inserted above");
            return Some((hash, signatures));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use linera_base::crypto::{AccountSecretKey, CryptoHash, ValidatorKeypair};
    use linera_chain::data_types::{LiteValue, LiteVote};

    use super::*;

    fn committee_of(size: usize) -> (Committee, Vec<ValidatorPublicKey>) {
        let keys: Vec<_> = (0..size)
            .map(|_| {
                (
                    ValidatorKeypair::generate().public_key,
                    AccountSecretKey::generate().public(),
                )
            })
            .collect();
        let public_keys = keys.iter().map(|(key, _)| *key).collect();
        (Committee::make_simple(keys), public_keys)
    }

    fn vote(chain_id: ChainId, value_hash: CryptoHash) -> LiteVote {
        LiteVote {
            value: LiteValue {
                value_hash,
                chain_id,
                kind: CertificateKind::Confirmed,
            },
            round: Round::Fast,
            unlocking_round: None,
            first_round: true,
            justification_commitment: None,
            signature: ValidatorSignature::sign_prehash(
                &ValidatorKeypair::generate().secret_key,
                value_hash,
            ),
        }
    }

    #[test]
    fn quorum_is_reached_once_enough_weight_agrees() {
        let chain_id = ChainId(CryptoHash::test_hash("chain"));
        let value_hash = CryptoHash::test_hash("confirmed-block");
        let (committee, keys) = committee_of(4);

        // Only 2 out of 4 equally-weighted validators agree: not a quorum yet.
        let votes = keys[..2]
            .iter()
            .map(|key| (*key, vote(chain_id, value_hash)));
        assert!(find_confirming_quorum(chain_id, votes, &committee).is_none());

        // 3 out of 4 is enough.
        let votes = keys[..3]
            .iter()
            .map(|key| (*key, vote(chain_id, value_hash)));
        let (hash, signatures) = find_confirming_quorum(chain_id, votes, &committee)
            .expect("3 out of 4 equally-weighted validators should reach the quorum threshold");
        assert_eq!(hash, value_hash);
        assert_eq!(signatures.len(), 3);
    }

    #[test]
    fn votes_for_a_different_chain_are_ignored() {
        let chain_id = ChainId(CryptoHash::test_hash("chain"));
        let other_chain_id = ChainId(CryptoHash::test_hash("other-chain"));
        let value_hash = CryptoHash::test_hash("confirmed-block");
        let (committee, keys) = committee_of(4);

        let votes = keys
            .iter()
            .map(|key| (*key, vote(other_chain_id, value_hash)));
        assert!(find_confirming_quorum(chain_id, votes, &committee).is_none());
    }

    #[test]
    fn a_split_vote_never_reaches_quorum_on_either_side() {
        let chain_id = ChainId(CryptoHash::test_hash("chain"));
        let hash_a = CryptoHash::test_hash("block-a");
        let hash_b = CryptoHash::test_hash("block-b");
        let (committee, keys) = committee_of(4);

        let votes = vec![
            (keys[0], vote(chain_id, hash_a)),
            (keys[1], vote(chain_id, hash_a)),
            (keys[2], vote(chain_id, hash_b)),
            (keys[3], vote(chain_id, hash_b)),
        ];
        assert!(find_confirming_quorum(chain_id, votes, &committee).is_none());
    }
}
