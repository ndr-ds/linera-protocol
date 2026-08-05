// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Dynamic chain-to-shard routing.
//!
//! A [`ShardRouter`] maps each chain to the shard (worker) currently responsible
//! for it. Chains without an explicit assignment fall back to the static,
//! hash-based default assignment (see
//! [`crate::config::default_shard_id`]). Assignments can be changed at runtime
//! to migrate chains between workers without restarting the validator.
//!
//! On workers, the router additionally acts as a *handover barrier*: every
//! request handler for a chain holds a read guard on the chain's entry for the
//! duration of the request, and a migration flips the entry through a write
//! guard. Acquiring the write guard therefore waits for all in-flight requests
//! for that chain to complete, and afterwards no new request can pass the
//! ownership check on the releasing worker.

use std::{collections::BTreeMap, sync::Arc};

use linera_base::{crypto::ValidatorPublicKey, identifiers::ChainId};
use tokio::sync::{OwnedRwLockReadGuard, RwLock};

use crate::config::{default_shard_id, ShardId};

/// A dynamic, concurrency-safe chain-to-shard routing table.
pub struct ShardRouter {
    /// The validator public key, used to randomize the default assignment.
    public_key: ValidatorPublicKey,
    /// The total number of shards of this validator, including elastic slots.
    num_shards: usize,
    /// The number of base shards over which the default (hash-based)
    /// assignment is computed. Elastic slots (shards beyond this prefix) only
    /// ever own explicitly assigned chains, so their workers can be started
    /// and stopped at runtime.
    num_base_shards: usize,
    /// Explicit per-chain assignments. Chains without an entry are routed to
    /// their default shard. Entries are created lazily; each holds the shard
    /// currently responsible for the chain.
    entries: papaya::HashMap<ChainId, Arc<RwLock<ShardId>>>,
}

impl ShardRouter {
    /// Creates a new router with no overrides. The default assignment hashes
    /// over the first `num_base_shards` shards; explicit assignments may target
    /// any of the `num_shards` shards.
    pub fn new(public_key: ValidatorPublicKey, num_shards: usize, num_base_shards: usize) -> Self {
        assert!(num_base_shards > 0 && num_base_shards <= num_shards);
        Self {
            public_key,
            num_shards,
            num_base_shards,
            entries: papaya::HashMap::new(),
        }
    }

    /// Returns the static, hash-based default shard for a chain.
    pub fn default_shard(&self, chain_id: ChainId) -> ShardId {
        default_shard_id(&self.public_key, self.num_base_shards, chain_id)
    }

    /// Returns the total number of shards.
    pub fn num_shards(&self) -> usize {
        self.num_shards
    }

    /// Returns the existing entry for the chain, or creates one initialized
    /// with the default assignment.
    fn entry(&self, chain_id: ChainId) -> Arc<RwLock<ShardId>> {
        let pin = self.entries.pin();
        pin.get_or_insert_with(chain_id, || {
            Arc::new(RwLock::new(self.default_shard(chain_id)))
        })
        .clone()
    }

    /// Returns the shard currently responsible for the chain.
    ///
    /// If the chain's entry is locked by an in-progress migration, this waits
    /// until the migration's write guard is released.
    pub async fn shard_for(&self, chain_id: ChainId) -> ShardId {
        let entry = {
            let pin = self.entries.pin();
            pin.get(&chain_id).cloned()
        };
        match entry {
            Some(entry) => *entry.read().await,
            None => self.default_shard(chain_id),
        }
    }

    /// Takes a read guard on the chain's routing entry, creating it if needed.
    ///
    /// The guard dereferences to the shard currently responsible for the chain.
    /// Request handlers hold it for the duration of request processing so that
    /// a concurrent migration (which takes the corresponding write guard) acts
    /// as a barrier for in-flight requests.
    pub async fn read_guard(&self, chain_id: ChainId) -> OwnedRwLockReadGuard<ShardId> {
        self.entry(chain_id).read_owned().await
    }

    /// Assigns the chain to the given shard.
    ///
    /// This waits until all read guards for the chain have been released, i.e.
    /// until all in-flight requests for the chain have completed, then flips
    /// the entry. After this returns, every subsequent [`Self::read_guard`] or
    /// [`Self::shard_for`] call observes the new assignment.
    pub async fn assign(&self, chain_id: ChainId, shard_id: ShardId) {
        let entry = self.entry(chain_id);
        let mut guard = entry.write_owned().await;
        *guard = shard_id;
    }

    /// Seeds the router with a persisted set of overrides. Intended for
    /// startup, before the router is shared with request handlers.
    pub async fn seed(&self, overrides: impl IntoIterator<Item = (ChainId, ShardId)>) {
        for (chain_id, shard_id) in overrides {
            self.assign(chain_id, shard_id).await;
        }
    }

    /// Returns the current set of non-default assignments, e.g. for
    /// persisting them to storage.
    pub async fn overrides(&self) -> BTreeMap<ChainId, ShardId> {
        let entries = {
            let pin = self.entries.pin();
            pin.iter()
                .map(|(chain_id, entry)| (*chain_id, entry.clone()))
                .collect::<Vec<_>>()
        };
        let mut overrides = BTreeMap::new();
        for (chain_id, entry) in entries {
            let shard_id = *entry.read().await;
            if shard_id != self.default_shard(chain_id) {
                overrides.insert(chain_id, shard_id);
            }
        }
        overrides
    }
}

#[cfg(test)]
mod tests {
    use linera_base::crypto::ValidatorKeypair;

    use super::*;

    fn router(num_shards: usize) -> ShardRouter {
        let keypair = ValidatorKeypair::generate();
        ShardRouter::new(keypair.public_key, num_shards, num_shards)
    }

    fn chain(index: u8) -> ChainId {
        ChainId(linera_base::crypto::CryptoHash::test_hash(format!(
            "chain{index}"
        )))
    }

    #[tokio::test]
    async fn test_default_assignment() {
        let router = router(4);
        let chain_id = chain(1);
        assert_eq!(
            router.shard_for(chain_id).await,
            router.default_shard(chain_id)
        );
        assert!(router.overrides().await.is_empty());
    }

    #[tokio::test]
    async fn test_override_and_reset() {
        let router = router(4);
        let chain_id = chain(1);
        let default = router.default_shard(chain_id);
        let target = (default + 1) % 4;

        router.assign(chain_id, target).await;
        assert_eq!(router.shard_for(chain_id).await, target);
        assert_eq!(
            router.overrides().await,
            BTreeMap::from([(chain_id, target)])
        );

        // Assigning back to the default clears the override.
        router.assign(chain_id, default).await;
        assert!(router.overrides().await.is_empty());
    }

    #[tokio::test]
    async fn test_elastic_slots_receive_no_default_assignments() {
        let keypair = ValidatorKeypair::generate();
        // Four shards, of which only the first two receive default traffic.
        let router = ShardRouter::new(keypair.public_key, 4, 2);
        for index in 0..64 {
            assert!(router.default_shard(chain(index)) < 2);
        }
        // Elastic slots can still be targeted explicitly.
        let chain_id = chain(1);
        router.assign(chain_id, 3).await;
        assert_eq!(router.shard_for(chain_id).await, 3);
    }

    #[tokio::test]
    async fn test_assign_waits_for_in_flight_guards() {
        let router = Arc::new(router(4));
        let chain_id = chain(1);
        let default = router.default_shard(chain_id);
        let guard = router.read_guard(chain_id).await;
        assert_eq!(*guard, default);

        let migration = {
            let router = router.clone();
            tokio::spawn(async move { router.assign(chain_id, default + 1).await })
        };
        // The migration cannot complete while the request guard is held.
        tokio::task::yield_now().await;
        assert!(!migration.is_finished());

        drop(guard);
        migration.await.unwrap();
        assert_eq!(router.shard_for(chain_id).await, default + 1);
    }
}
