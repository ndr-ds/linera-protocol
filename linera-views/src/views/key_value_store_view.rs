// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! We implement two types:
//! 1) The first type `KeyValueStoreView` implements View and the function of `KeyValueStore`.
//!
//! 2) The second type `ViewContainer` encapsulates `KeyValueStoreView` and provides the following functionalities:
//!    * The `Clone` trait
//!    * a `write_batch` that takes a `&self` instead of a `&mut self`
//!    * a `write_batch` that writes in the context instead of writing of the view.
//!
//! Currently, that second type is only used for tests.
//!
//! Key tags to create the sub-keys of a `KeyValueStoreView` on top of the base key.

use std::{collections::BTreeMap, fmt::Debug, mem, ops::Bound::Included, sync::Mutex};

#[cfg(with_metrics)]
use linera_base::prometheus_util::MeasureLatency as _;
use linera_base::{data_types::ArithmeticError, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    batch::{Batch, WriteOperation},
    common::{
        from_bytes_option, from_bytes_option_or_default, get_interval, get_upper_bound,
        DeletionSet, HasherOutput, SuffixClosedSetIterator, Update,
    },
    context::Context,
    map_view::ByteMapView,
    store::ReadableKeyValueStore,
    views::{ClonableView, HashableView, Hasher, View, ViewError, MIN_VIEW_TAG},
};

#[cfg(with_metrics)]
mod metrics {
    use std::sync::LazyLock;

    use linera_base::prometheus_util::{exponential_bucket_latencies, register_histogram_vec};
    use prometheus::HistogramVec;

    /// The latency of hash computation
    pub static KEY_VALUE_STORE_VIEW_HASH_LATENCY: LazyLock<HistogramVec> = LazyLock::new(|| {
        register_histogram_vec(
            "key_value_store_view_hash_latency",
            "KeyValueStoreView hash latency",
            &[],
            exponential_bucket_latencies(5.0),
        )
    });

    /// The latency of get operation
    pub static KEY_VALUE_STORE_VIEW_GET_LATENCY: LazyLock<HistogramVec> = LazyLock::new(|| {
        register_histogram_vec(
            "key_value_store_view_get_latency",
            "KeyValueStoreView get latency",
            &[],
            exponential_bucket_latencies(5.0),
        )
    });

    /// The latency of multi get
    pub static KEY_VALUE_STORE_VIEW_MULTI_GET_LATENCY: LazyLock<HistogramVec> =
        LazyLock::new(|| {
            register_histogram_vec(
                "key_value_store_view_multi_get_latency",
                "KeyValueStoreView multi get latency",
                &[],
                exponential_bucket_latencies(5.0),
            )
        });

    /// The latency of contains key
    pub static KEY_VALUE_STORE_VIEW_CONTAINS_KEY_LATENCY: LazyLock<HistogramVec> =
        LazyLock::new(|| {
            register_histogram_vec(
                "key_value_store_view_contains_key_latency",
                "KeyValueStoreView contains key latency",
                &[],
                exponential_bucket_latencies(5.0),
            )
        });

    /// The latency of contains keys
    pub static KEY_VALUE_STORE_VIEW_CONTAINS_KEYS_LATENCY: LazyLock<HistogramVec> =
        LazyLock::new(|| {
            register_histogram_vec(
                "key_value_store_view_contains_keys_latency",
                "KeyValueStoreView contains keys latency",
                &[],
                exponential_bucket_latencies(5.0),
            )
        });

    /// The latency of find keys by prefix operation
    pub static KEY_VALUE_STORE_VIEW_FIND_KEYS_BY_PREFIX_LATENCY: LazyLock<HistogramVec> =
        LazyLock::new(|| {
            register_histogram_vec(
                "key_value_store_view_find_keys_by_prefix_latency",
                "KeyValueStoreView find keys by prefix latency",
                &[],
                exponential_bucket_latencies(5.0),
            )
        });

    /// The latency of find key values by prefix operation
    pub static KEY_VALUE_STORE_VIEW_FIND_KEY_VALUES_BY_PREFIX_LATENCY: LazyLock<HistogramVec> =
        LazyLock::new(|| {
            register_histogram_vec(
                "key_value_store_view_find_key_values_by_prefix_latency",
                "KeyValueStoreView find key values by prefix latency",
                &[],
                exponential_bucket_latencies(5.0),
            )
        });

    /// The latency of write batch operation
    pub static KEY_VALUE_STORE_VIEW_WRITE_BATCH_LATENCY: LazyLock<HistogramVec> =
        LazyLock::new(|| {
            register_histogram_vec(
                "key_value_store_view_write_batch_latency",
                "KeyValueStoreView write batch latency",
                &[],
                exponential_bucket_latencies(5.0),
            )
        });
}

#[cfg(with_testing)]
use {
    crate::store::{KeyValueStoreError, WithError, WritableKeyValueStore},
    async_lock::RwLock,
    std::sync::Arc,
    thiserror::Error,
};

#[repr(u8)]
enum KeyTag {
    /// Prefix for the indices of the view.
    Index = MIN_VIEW_TAG,
    /// The total stored size
    TotalSize,
    /// The prefix where the sizes are being stored
    Sizes,
    /// Prefix for the hash.
    Hash,
}

/// A pair containing the key and value size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeData {
    /// The size of the key
    pub key: u32,
    /// The size of the value
    pub value: u32,
}

impl SizeData {
    /// Sums both terms
    pub fn sum(&mut self) -> u32 {
        self.key + self.value
    }

    /// Adds a size to `self`
    pub fn add_assign(&mut self, size: SizeData) -> Result<(), ViewError> {
        self.key = self
            .key
            .checked_add(size.key)
            .ok_or(ViewError::ArithmeticError(ArithmeticError::Overflow))?;
        self.value = self
            .value
            .checked_add(size.value)
            .ok_or(ViewError::ArithmeticError(ArithmeticError::Overflow))?;
        Ok(())
    }

    /// Subtracts a size from `self`
    pub fn sub_assign(&mut self, size: SizeData) {
        self.key -= size.key;
        self.value -= size.value;
    }
}

/// A view that represents the functions of `KeyValueStore`.
///
/// Comment on the data set:
/// In order to work, the view needs to store the updates and deleted prefixes.
/// The updates and deleted prefixes have to be coherent. This means:
/// * If an index is deleted by one in deleted prefixes then it should not be present
///   in the updates at all.
/// * [`DeletePrefix::key_prefix`][entry1] should not dominate anyone. That is if we have `[0,2]`
///   then we should not have `[0,2,3]` since it would be dominated by the preceding.
///
/// With that we have:
/// * in order to test if an `index` is deleted by a prefix we compute the highest deleted prefix `dp`
///   such that `dp <= index`.
///   If `dp` is indeed a prefix then we conclude that `index` is deleted, otherwise not.
///   The no domination is essential here.
///
/// [entry1]: crate::batch::WriteOperation::DeletePrefix
#[derive(Debug)]
pub struct KeyValueStoreView<C> {
    context: C,
    deletion_set: DeletionSet,
    updates: BTreeMap<Vec<u8>, Update<Vec<u8>>>,
    stored_total_size: SizeData,
    total_size: SizeData,
    sizes: ByteMapView<C, u32>,
    stored_hash: Option<HasherOutput>,
    hash: Mutex<Option<HasherOutput>>,
}

impl<C: Context> View for KeyValueStoreView<C> {
    const NUM_INIT_KEYS: usize = 2 + ByteMapView::<C, u32>::NUM_INIT_KEYS;

    type Context = C;

    fn context(&self) -> &C {
        &self.context
    }

    fn pre_load(context: &C) -> Result<Vec<Vec<u8>>, ViewError> {
        let key_hash = context.base_key().base_tag(KeyTag::Hash as u8);
        let key_total_size = context.base_key().base_tag(KeyTag::TotalSize as u8);
        let mut v = vec![key_hash, key_total_size];
        let base_key = context.base_key().base_tag(KeyTag::Sizes as u8);
        let context_sizes = context.clone_with_base_key(base_key);
        v.extend(ByteMapView::<C, u32>::pre_load(&context_sizes)?);
        Ok(v)
    }

    fn post_load(context: C, values: &[Option<Vec<u8>>]) -> Result<Self, ViewError> {
        let hash = from_bytes_option(values.first().ok_or(ViewError::PostLoadValuesError)?)?;
        let total_size =
            from_bytes_option_or_default(values.get(1).ok_or(ViewError::PostLoadValuesError)?)?;
        let base_key = context.base_key().base_tag(KeyTag::Sizes as u8);
        let context_sizes = context.clone_with_base_key(base_key);
        let sizes = ByteMapView::post_load(
            context_sizes,
            values.get(2..).ok_or(ViewError::PostLoadValuesError)?,
        )?;
        Ok(Self {
            context,
            deletion_set: DeletionSet::new(),
            updates: BTreeMap::new(),
            stored_total_size: total_size,
            total_size,
            sizes,
            stored_hash: hash,
            hash: Mutex::new(hash),
        })
    }

    async fn load(context: C) -> Result<Self, ViewError> {
        let keys = Self::pre_load(&context)?;
        let values = context.store().read_multi_values_bytes(keys).await?;
        Self::post_load(context, &values)
    }

    fn rollback(&mut self) {
        self.deletion_set.rollback();
        self.updates.clear();
        self.total_size = self.stored_total_size;
        self.sizes.rollback();
        *self.hash.get_mut().unwrap() = self.stored_hash;
    }

    async fn has_pending_changes(&self) -> bool {
        if self.deletion_set.has_pending_changes() {
            return true;
        }
        if !self.updates.is_empty() {
            return true;
        }
        if self.stored_total_size != self.total_size {
            return true;
        }
        if self.sizes.has_pending_changes().await {
            return true;
        }
        let hash = self.hash.lock().unwrap();
        self.stored_hash != *hash
    }

    fn flush(&mut self, batch: &mut Batch) -> Result<bool, ViewError> {
        let mut delete_view = false;
        if self.deletion_set.delete_storage_first {
            delete_view = true;
            self.stored_total_size = SizeData::default();
            batch.delete_key_prefix(self.context.base_key().bytes.clone());
            for (index, update) in mem::take(&mut self.updates) {
                if let Update::Set(value) = update {
                    let key = self
                        .context
                        .base_key()
                        .base_tag_index(KeyTag::Index as u8, &index);
                    batch.put_key_value_bytes(key, value);
                    delete_view = false;
                }
            }
            self.stored_hash = None
        } else {
            for index in mem::take(&mut self.deletion_set.deleted_prefixes) {
                let key = self
                    .context
                    .base_key()
                    .base_tag_index(KeyTag::Index as u8, &index);
                batch.delete_key_prefix(key);
            }
            for (index, update) in mem::take(&mut self.updates) {
                let key = self
                    .context
                    .base_key()
                    .base_tag_index(KeyTag::Index as u8, &index);
                match update {
                    Update::Removed => batch.delete_key(key),
                    Update::Set(value) => batch.put_key_value_bytes(key, value),
                }
            }
        }
        self.sizes.flush(batch)?;
        let hash = *self.hash.get_mut().unwrap();
        if self.stored_hash != hash {
            let key = self.context.base_key().base_tag(KeyTag::Hash as u8);
            match hash {
                None => batch.delete_key(key),
                Some(hash) => batch.put_key_value(key, &hash)?,
            }
            self.stored_hash = hash;
        }
        if self.stored_total_size != self.total_size {
            let key = self.context.base_key().base_tag(KeyTag::TotalSize as u8);
            batch.put_key_value(key, &self.total_size)?;
            self.stored_total_size = self.total_size;
        }
        self.deletion_set.delete_storage_first = false;
        Ok(delete_view)
    }

    fn clear(&mut self) {
        self.deletion_set.clear();
        self.updates.clear();
        self.total_size = SizeData::default();
        self.sizes.clear();
        *self.hash.get_mut().unwrap() = None;
    }
}

impl<C: Context> ClonableView for KeyValueStoreView<C> {
    fn clone_unchecked(&mut self) -> Result<Self, ViewError> {
        Ok(KeyValueStoreView {
            context: self.context.clone(),
            deletion_set: self.deletion_set.clone(),
            updates: self.updates.clone(),
            stored_total_size: self.stored_total_size,
            total_size: self.total_size,
            sizes: self.sizes.clone_unchecked()?,
            stored_hash: self.stored_hash,
            hash: Mutex::new(*self.hash.get_mut().unwrap()),
        })
    }
}

impl<C: Context> KeyValueStoreView<C> {
    fn max_key_size(&self) -> usize {
        let prefix_len = self.context.base_key().bytes.len();
        <C::Store as ReadableKeyValueStore>::MAX_KEY_SIZE - 1 - prefix_len
    }

    /// Getting the total sizes that will be used for keys and values when stored
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::{KeyValueStoreView, SizeData};
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// let total_size = view.total_size();
    /// assert_eq!(total_size, SizeData::default());
    /// # })
    /// ```
    pub fn total_size(&self) -> SizeData {
        self.total_size
    }

    /// Applies the function f over all indices. If the function f returns
    /// false, then the loop ends prematurely.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![0]).await.unwrap();
    /// view.insert(vec![0, 2], vec![0]).await.unwrap();
    /// view.insert(vec![0, 3], vec![0]).await.unwrap();
    /// let mut count = 0;
    /// view.for_each_index_while(|_key| {
    ///     count += 1;
    ///     Ok(count < 2)
    /// })
    /// .await
    /// .unwrap();
    /// assert_eq!(count, 2);
    /// # })
    /// ```
    pub async fn for_each_index_while<F>(&self, mut f: F) -> Result<(), ViewError>
    where
        F: FnMut(&[u8]) -> Result<bool, ViewError> + Send,
    {
        let key_prefix = self.context.base_key().base_tag(KeyTag::Index as u8);
        let mut updates = self.updates.iter();
        let mut update = updates.next();
        if !self.deletion_set.delete_storage_first {
            let mut suffix_closed_set =
                SuffixClosedSetIterator::new(0, self.deletion_set.deleted_prefixes.iter());
            for index in self
                .context
                .store()
                .find_keys_by_prefix(&key_prefix)
                .await?
            {
                loop {
                    match update {
                        Some((key, value)) if key <= &index => {
                            if let Update::Set(_) = value {
                                if !f(key)? {
                                    return Ok(());
                                }
                            }
                            update = updates.next();
                            if key == &index {
                                break;
                            }
                        }
                        _ => {
                            if !suffix_closed_set.find_key(&index) && !f(&index)? {
                                return Ok(());
                            }
                            break;
                        }
                    }
                }
            }
        }
        while let Some((key, value)) = update {
            if let Update::Set(_) = value {
                if !f(key)? {
                    return Ok(());
                }
            }
            update = updates.next();
        }
        Ok(())
    }

    /// Applies the function f over all indices.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![0]).await.unwrap();
    /// view.insert(vec![0, 2], vec![0]).await.unwrap();
    /// view.insert(vec![0, 3], vec![0]).await.unwrap();
    /// let mut count = 0;
    /// view.for_each_index(|_key| {
    ///     count += 1;
    ///     Ok(())
    /// })
    /// .await
    /// .unwrap();
    /// assert_eq!(count, 3);
    /// # })
    /// ```
    pub async fn for_each_index<F>(&self, mut f: F) -> Result<(), ViewError>
    where
        F: FnMut(&[u8]) -> Result<(), ViewError> + Send,
    {
        self.for_each_index_while(|key| {
            f(key)?;
            Ok(true)
        })
        .await
    }

    /// Applies the function f over all index/value pairs.
    /// If the function f returns false then the loop ends prematurely.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![0]).await.unwrap();
    /// view.insert(vec![0, 2], vec![0]).await.unwrap();
    /// let mut values = Vec::new();
    /// view.for_each_index_value_while(|_key, value| {
    ///     values.push(value.to_vec());
    ///     Ok(values.len() < 1)
    /// })
    /// .await
    /// .unwrap();
    /// assert_eq!(values, vec![vec![0]]);
    /// # })
    /// ```
    pub async fn for_each_index_value_while<F>(&self, mut f: F) -> Result<(), ViewError>
    where
        F: FnMut(&[u8], &[u8]) -> Result<bool, ViewError> + Send,
    {
        let key_prefix = self.context.base_key().base_tag(KeyTag::Index as u8);
        let mut updates = self.updates.iter();
        let mut update = updates.next();
        if !self.deletion_set.delete_storage_first {
            let mut suffix_closed_set =
                SuffixClosedSetIterator::new(0, self.deletion_set.deleted_prefixes.iter());
            for entry in self
                .context
                .store()
                .find_key_values_by_prefix(&key_prefix)
                .await?
            {
                let (index, index_val) = entry;
                loop {
                    match update {
                        Some((key, value)) if key <= &index => {
                            if let Update::Set(value) = value {
                                if !f(key, value)? {
                                    return Ok(());
                                }
                            }
                            update = updates.next();
                            if key == &index {
                                break;
                            }
                        }
                        _ => {
                            if !suffix_closed_set.find_key(&index) && !f(&index, &index_val)? {
                                return Ok(());
                            }
                            break;
                        }
                    }
                }
            }
        }
        while let Some((key, value)) = update {
            if let Update::Set(value) = value {
                if !f(key, value)? {
                    return Ok(());
                }
            }
            update = updates.next();
        }
        Ok(())
    }

    /// Applies the function f over all index/value pairs.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![0]).await.unwrap();
    /// view.insert(vec![0, 2], vec![0]).await.unwrap();
    /// let mut part_keys = Vec::new();
    /// view.for_each_index_while(|key| {
    ///     part_keys.push(key.to_vec());
    ///     Ok(part_keys.len() < 1)
    /// })
    /// .await
    /// .unwrap();
    /// assert_eq!(part_keys, vec![vec![0, 1]]);
    /// # })
    /// ```
    pub async fn for_each_index_value<F>(&self, mut f: F) -> Result<(), ViewError>
    where
        F: FnMut(&[u8], &[u8]) -> Result<(), ViewError> + Send,
    {
        self.for_each_index_value_while(|key, value| {
            f(key, value)?;
            Ok(true)
        })
        .await
    }

    /// Returns the list of indices in lexicographic order.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![0]).await.unwrap();
    /// view.insert(vec![0, 2], vec![0]).await.unwrap();
    /// let indices = view.indices().await.unwrap();
    /// assert_eq!(indices, vec![vec![0, 1], vec![0, 2]]);
    /// # })
    /// ```
    pub async fn indices(&self) -> Result<Vec<Vec<u8>>, ViewError> {
        let mut indices = Vec::new();
        self.for_each_index(|index| {
            indices.push(index.to_vec());
            Ok(())
        })
        .await?;
        Ok(indices)
    }

    /// Returns the list of indices and values in lexicographic order.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![0]).await.unwrap();
    /// view.insert(vec![0, 2], vec![0]).await.unwrap();
    /// let key_values = view.indices().await.unwrap();
    /// assert_eq!(key_values, vec![vec![0, 1], vec![0, 2]]);
    /// # })
    /// ```
    pub async fn index_values(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ViewError> {
        let mut index_values = Vec::new();
        self.for_each_index_value(|index, value| {
            index_values.push((index.to_vec(), value.to_vec()));
            Ok(())
        })
        .await?;
        Ok(index_values)
    }

    /// Returns the number of entries.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![0]).await.unwrap();
    /// view.insert(vec![0, 2], vec![0]).await.unwrap();
    /// let count = view.count().await.unwrap();
    /// assert_eq!(count, 2);
    /// # })
    /// ```
    pub async fn count(&self) -> Result<usize, ViewError> {
        let mut count = 0;
        self.for_each_index(|_index| {
            count += 1;
            Ok(())
        })
        .await?;
        Ok(count)
    }

    /// Obtains the value at the given index, if any.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![42]).await.unwrap();
    /// assert_eq!(view.get(&[0, 1]).await.unwrap(), Some(vec![42]));
    /// assert_eq!(view.get(&[0, 2]).await.unwrap(), None);
    /// # })
    /// ```
    pub async fn get(&self, index: &[u8]) -> Result<Option<Vec<u8>>, ViewError> {
        #[cfg(with_metrics)]
        let _latency = metrics::KEY_VALUE_STORE_VIEW_GET_LATENCY.measure_latency();
        ensure!(index.len() <= self.max_key_size(), ViewError::KeyTooLong);
        if let Some(update) = self.updates.get(index) {
            let value = match update {
                Update::Removed => None,
                Update::Set(value) => Some(value.clone()),
            };
            return Ok(value);
        }
        if self.deletion_set.contains_prefix_of(index) {
            return Ok(None);
        }
        let key = self
            .context
            .base_key()
            .base_tag_index(KeyTag::Index as u8, index);
        Ok(self.context.store().read_value_bytes(&key).await?)
    }

    /// Tests whether the store contains a specific index.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![42]).await.unwrap();
    /// assert!(view.contains_key(&[0, 1]).await.unwrap());
    /// assert!(!view.contains_key(&[0, 2]).await.unwrap());
    /// # })
    /// ```
    pub async fn contains_key(&self, index: &[u8]) -> Result<bool, ViewError> {
        #[cfg(with_metrics)]
        let _latency = metrics::KEY_VALUE_STORE_VIEW_CONTAINS_KEY_LATENCY.measure_latency();
        ensure!(index.len() <= self.max_key_size(), ViewError::KeyTooLong);
        if let Some(update) = self.updates.get(index) {
            let test = match update {
                Update::Removed => false,
                Update::Set(_value) => true,
            };
            return Ok(test);
        }
        if self.deletion_set.contains_prefix_of(index) {
            return Ok(false);
        }
        let key = self
            .context
            .base_key()
            .base_tag_index(KeyTag::Index as u8, index);
        Ok(self.context.store().contains_key(&key).await?)
    }

    /// Tests whether the view contains a range of indices
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![42]).await.unwrap();
    /// let keys = vec![vec![0, 1], vec![0, 2]];
    /// let results = view.contains_keys(keys).await.unwrap();
    /// assert_eq!(results, vec![true, false]);
    /// # })
    /// ```
    pub async fn contains_keys(&self, indices: Vec<Vec<u8>>) -> Result<Vec<bool>, ViewError> {
        #[cfg(with_metrics)]
        let _latency = metrics::KEY_VALUE_STORE_VIEW_CONTAINS_KEYS_LATENCY.measure_latency();
        let mut results = Vec::with_capacity(indices.len());
        let mut missed_indices = Vec::new();
        let mut vector_query = Vec::new();
        for (i, index) in indices.into_iter().enumerate() {
            ensure!(index.len() <= self.max_key_size(), ViewError::KeyTooLong);
            if let Some(update) = self.updates.get(&index) {
                let value = match update {
                    Update::Removed => false,
                    Update::Set(_) => true,
                };
                results.push(value);
            } else {
                results.push(false);
                if !self.deletion_set.contains_prefix_of(&index) {
                    missed_indices.push(i);
                    let key = self
                        .context
                        .base_key()
                        .base_tag_index(KeyTag::Index as u8, &index);
                    vector_query.push(key);
                }
            }
        }
        let values = self.context.store().contains_keys(vector_query).await?;
        for (i, value) in missed_indices.into_iter().zip(values) {
            results[i] = value;
        }
        Ok(results)
    }

    /// Obtains the values of a range of indices
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![42]).await.unwrap();
    /// assert_eq!(
    ///     view.multi_get(vec![vec![0, 1], vec![0, 2]]).await.unwrap(),
    ///     vec![Some(vec![42]), None]
    /// );
    /// # })
    /// ```
    pub async fn multi_get(
        &self,
        indices: Vec<Vec<u8>>,
    ) -> Result<Vec<Option<Vec<u8>>>, ViewError> {
        #[cfg(with_metrics)]
        let _latency = metrics::KEY_VALUE_STORE_VIEW_MULTI_GET_LATENCY.measure_latency();
        let mut result = Vec::with_capacity(indices.len());
        let mut missed_indices = Vec::new();
        let mut vector_query = Vec::new();
        for (i, index) in indices.into_iter().enumerate() {
            ensure!(index.len() <= self.max_key_size(), ViewError::KeyTooLong);
            if let Some(update) = self.updates.get(&index) {
                let value = match update {
                    Update::Removed => None,
                    Update::Set(value) => Some(value.clone()),
                };
                result.push(value);
            } else {
                result.push(None);
                if !self.deletion_set.contains_prefix_of(&index) {
                    missed_indices.push(i);
                    let key = self
                        .context
                        .base_key()
                        .base_tag_index(KeyTag::Index as u8, &index);
                    vector_query.push(key);
                }
            }
        }
        let values = self
            .context
            .store()
            .read_multi_values_bytes(vector_query)
            .await?;
        for (i, value) in missed_indices.into_iter().zip(values) {
            result[i] = value;
        }
        Ok(result)
    }

    /// Applies the given batch of `crate::common::WriteOperation`.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::batch::Batch;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![34]).await.unwrap();
    /// view.insert(vec![3, 4], vec![42]).await.unwrap();
    /// let mut batch = Batch::new();
    /// batch.delete_key_prefix(vec![0]);
    /// view.write_batch(batch).await.unwrap();
    /// let key_values = view.find_key_values_by_prefix(&[0]).await.unwrap();
    /// assert_eq!(key_values, vec![]);
    /// # })
    /// ```
    pub async fn write_batch(&mut self, batch: Batch) -> Result<(), ViewError> {
        #[cfg(with_metrics)]
        let _latency = metrics::KEY_VALUE_STORE_VIEW_WRITE_BATCH_LATENCY.measure_latency();
        *self.hash.get_mut().unwrap() = None;
        let max_key_size = self.max_key_size();
        for operation in batch.operations {
            match operation {
                WriteOperation::Delete { key } => {
                    ensure!(key.len() <= max_key_size, ViewError::KeyTooLong);
                    if let Some(value) = self.sizes.get(&key).await? {
                        let entry_size = SizeData {
                            key: u32::try_from(key.len()).map_err(|_| ArithmeticError::Overflow)?,
                            value,
                        };
                        self.total_size.sub_assign(entry_size);
                    }
                    self.sizes.remove(key.clone());
                    if self.deletion_set.contains_prefix_of(&key) {
                        // Optimization: No need to mark `short_key` for deletion as we are going to remove all the keys at once.
                        self.updates.remove(&key);
                    } else {
                        self.updates.insert(key, Update::Removed);
                    }
                }
                WriteOperation::Put { key, value } => {
                    ensure!(key.len() <= max_key_size, ViewError::KeyTooLong);
                    let entry_size = SizeData {
                        key: key.len() as u32,
                        value: value.len() as u32,
                    };
                    self.total_size.add_assign(entry_size)?;
                    if let Some(value) = self.sizes.get(&key).await? {
                        let entry_size = SizeData {
                            key: key.len() as u32,
                            value,
                        };
                        self.total_size.sub_assign(entry_size);
                    }
                    self.sizes.insert(key.clone(), entry_size.value);
                    self.updates.insert(key, Update::Set(value));
                }
                WriteOperation::DeletePrefix { key_prefix } => {
                    ensure!(key_prefix.len() <= max_key_size, ViewError::KeyTooLong);
                    let key_list = self
                        .updates
                        .range(get_interval(key_prefix.clone()))
                        .map(|x| x.0.to_vec())
                        .collect::<Vec<_>>();
                    for key in key_list {
                        self.updates.remove(&key);
                    }
                    let key_values = self.sizes.key_values_by_prefix(key_prefix.clone()).await?;
                    for (key, value) in key_values {
                        let entry_size = SizeData {
                            key: key.len() as u32,
                            value,
                        };
                        self.total_size.sub_assign(entry_size);
                        self.sizes.remove(key);
                    }
                    self.sizes.remove_by_prefix(key_prefix.clone());
                    self.deletion_set.insert_key_prefix(key_prefix);
                }
            }
        }
        Ok(())
    }

    /// Sets or inserts a value.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![34]).await.unwrap();
    /// assert_eq!(view.get(&[0, 1]).await.unwrap(), Some(vec![34]));
    /// # })
    /// ```
    pub async fn insert(&mut self, index: Vec<u8>, value: Vec<u8>) -> Result<(), ViewError> {
        let mut batch = Batch::new();
        batch.put_key_value_bytes(index, value);
        self.write_batch(batch).await
    }

    /// Removes a value. If absent then the action has no effect.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![34]).await.unwrap();
    /// view.remove(vec![0, 1]).await.unwrap();
    /// assert_eq!(view.get(&[0, 1]).await.unwrap(), None);
    /// # })
    /// ```
    pub async fn remove(&mut self, index: Vec<u8>) -> Result<(), ViewError> {
        let mut batch = Batch::new();
        batch.delete_key(index);
        self.write_batch(batch).await
    }

    /// Deletes a key prefix.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![34]).await.unwrap();
    /// view.remove_by_prefix(vec![0]).await.unwrap();
    /// assert_eq!(view.get(&[0, 1]).await.unwrap(), None);
    /// # })
    /// ```
    pub async fn remove_by_prefix(&mut self, key_prefix: Vec<u8>) -> Result<(), ViewError> {
        let mut batch = Batch::new();
        batch.delete_key_prefix(key_prefix);
        self.write_batch(batch).await
    }

    /// Iterates over all the keys matching the given prefix. The prefix is not included in the returned keys.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![34]).await.unwrap();
    /// view.insert(vec![3, 4], vec![42]).await.unwrap();
    /// let keys = view.find_keys_by_prefix(&[0]).await.unwrap();
    /// assert_eq!(keys, vec![vec![1]]);
    /// # })
    /// ```
    pub async fn find_keys_by_prefix(&self, key_prefix: &[u8]) -> Result<Vec<Vec<u8>>, ViewError> {
        #[cfg(with_metrics)]
        let _latency = metrics::KEY_VALUE_STORE_VIEW_FIND_KEYS_BY_PREFIX_LATENCY.measure_latency();
        ensure!(
            key_prefix.len() <= self.max_key_size(),
            ViewError::KeyTooLong
        );
        let len = key_prefix.len();
        let key_prefix_full = self
            .context
            .base_key()
            .base_tag_index(KeyTag::Index as u8, key_prefix);
        let mut keys = Vec::new();
        let key_prefix_upper = get_upper_bound(key_prefix);
        let mut updates = self
            .updates
            .range((Included(key_prefix.to_vec()), key_prefix_upper));
        let mut update = updates.next();
        if !self.deletion_set.delete_storage_first {
            let mut suffix_closed_set =
                SuffixClosedSetIterator::new(0, self.deletion_set.deleted_prefixes.iter());
            for key in self
                .context
                .store()
                .find_keys_by_prefix(&key_prefix_full)
                .await?
            {
                loop {
                    match update {
                        Some((update_key, update_value))
                            if &update_key[len..] <= key.as_slice() =>
                        {
                            if let Update::Set(_) = update_value {
                                keys.push(update_key[len..].to_vec());
                            }
                            update = updates.next();
                            if update_key[len..] == key[..] {
                                break;
                            }
                        }
                        _ => {
                            let mut key_with_prefix = key_prefix.to_vec();
                            key_with_prefix.extend_from_slice(&key);
                            if !suffix_closed_set.find_key(&key_with_prefix) {
                                keys.push(key);
                            }
                            break;
                        }
                    }
                }
            }
        }
        while let Some((update_key, update_value)) = update {
            if let Update::Set(_) = update_value {
                let update_key = update_key[len..].to_vec();
                keys.push(update_key);
            }
            update = updates.next();
        }
        Ok(keys)
    }

    /// Iterates over all the key-value pairs, for keys matching the given prefix. The
    /// prefix is not included in the returned keys.
    /// ```rust
    /// # tokio_test::block_on(async {
    /// # use linera_views::context::MemoryContext;
    /// # use linera_views::key_value_store_view::KeyValueStoreView;
    /// # use linera_views::views::View;
    /// # let context = MemoryContext::new_for_testing(());
    /// let mut view = KeyValueStoreView::load(context).await.unwrap();
    /// view.insert(vec![0, 1], vec![34]).await.unwrap();
    /// view.insert(vec![3, 4], vec![42]).await.unwrap();
    /// let key_values = view.find_key_values_by_prefix(&[0]).await.unwrap();
    /// assert_eq!(key_values, vec![(vec![1], vec![34])]);
    /// # })
    /// ```
    pub async fn find_key_values_by_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ViewError> {
        #[cfg(with_metrics)]
        let _latency =
            metrics::KEY_VALUE_STORE_VIEW_FIND_KEY_VALUES_BY_PREFIX_LATENCY.measure_latency();
        ensure!(
            key_prefix.len() <= self.max_key_size(),
            ViewError::KeyTooLong
        );
        let len = key_prefix.len();
        let key_prefix_full = self
            .context
            .base_key()
            .base_tag_index(KeyTag::Index as u8, key_prefix);
        let mut key_values = Vec::new();
        let key_prefix_upper = get_upper_bound(key_prefix);
        let mut updates = self
            .updates
            .range((Included(key_prefix.to_vec()), key_prefix_upper));
        let mut update = updates.next();
        if !self.deletion_set.delete_storage_first {
            let mut suffix_closed_set =
                SuffixClosedSetIterator::new(0, self.deletion_set.deleted_prefixes.iter());
            for entry in self
                .context
                .store()
                .find_key_values_by_prefix(&key_prefix_full)
                .await?
            {
                let (key, value) = entry;
                loop {
                    match update {
                        Some((update_key, update_value)) if update_key[len..] <= key[..] => {
                            if let Update::Set(update_value) = update_value {
                                let key_value = (update_key[len..].to_vec(), update_value.to_vec());
                                key_values.push(key_value);
                            }
                            update = updates.next();
                            if update_key[len..] == key[..] {
                                break;
                            }
                        }
                        _ => {
                            let mut key_with_prefix = key_prefix.to_vec();
                            key_with_prefix.extend_from_slice(&key);
                            if !suffix_closed_set.find_key(&key_with_prefix) {
                                key_values.push((key, value));
                            }
                            break;
                        }
                    }
                }
            }
        }
        while let Some((update_key, update_value)) = update {
            if let Update::Set(update_value) = update_value {
                let key_value = (update_key[len..].to_vec(), update_value.to_vec());
                key_values.push(key_value);
            }
            update = updates.next();
        }
        Ok(key_values)
    }

    async fn compute_hash(&self) -> Result<<sha3::Sha3_256 as Hasher>::Output, ViewError> {
        #[cfg(with_metrics)]
        let _hash_latency = metrics::KEY_VALUE_STORE_VIEW_HASH_LATENCY.measure_latency();
        let mut hasher = sha3::Sha3_256::default();
        let mut count = 0u32;
        self.for_each_index_value(|index, value| -> Result<(), ViewError> {
            count += 1;
            hasher.update_with_bytes(index)?;
            hasher.update_with_bytes(value)?;
            Ok(())
        })
        .await?;
        hasher.update_with_bcs_bytes(&count)?;
        Ok(hasher.finalize())
    }
}

impl<C: Context> HashableView for KeyValueStoreView<C> {
    type Hasher = sha3::Sha3_256;

    async fn hash_mut(&mut self) -> Result<<Self::Hasher as Hasher>::Output, ViewError> {
        let hash = *self.hash.get_mut().unwrap();
        match hash {
            Some(hash) => Ok(hash),
            None => {
                let new_hash = self.compute_hash().await?;
                let hash = self.hash.get_mut().unwrap();
                *hash = Some(new_hash);
                Ok(new_hash)
            }
        }
    }

    async fn hash(&self) -> Result<<Self::Hasher as Hasher>::Output, ViewError> {
        let hash = *self.hash.lock().unwrap();
        match hash {
            Some(hash) => Ok(hash),
            None => {
                let new_hash = self.compute_hash().await?;
                let mut hash = self.hash.lock().unwrap();
                *hash = Some(new_hash);
                Ok(new_hash)
            }
        }
    }
}

/// A virtual DB client using a `KeyValueStoreView` as a backend (testing only).
#[cfg(with_testing)]
#[derive(Debug, Clone)]
pub struct ViewContainer<C> {
    view: Arc<RwLock<KeyValueStoreView<C>>>,
}

#[cfg(with_testing)]
impl<C> WithError for ViewContainer<C> {
    type Error = ViewContainerError;
}

#[cfg(with_testing)]
/// The error type for [`ViewContainer`] operations.
#[derive(Error, Debug)]
pub enum ViewContainerError {
    /// View error.
    #[error(transparent)]
    ViewError(#[from] ViewError),

    /// BCS serialization error.
    #[error(transparent)]
    BcsError(#[from] bcs::Error),
}

#[cfg(with_testing)]
impl KeyValueStoreError for ViewContainerError {
    const BACKEND: &'static str = "view_container";
}

#[cfg(with_testing)]
impl<C: Context> ReadableKeyValueStore for ViewContainer<C> {
    const MAX_KEY_SIZE: usize = <C::Store as ReadableKeyValueStore>::MAX_KEY_SIZE;

    fn max_stream_queries(&self) -> usize {
        1
    }

    async fn read_value_bytes(&self, key: &[u8]) -> Result<Option<Vec<u8>>, ViewContainerError> {
        let view = self.view.read().await;
        Ok(view.get(key).await?)
    }

    async fn contains_key(&self, key: &[u8]) -> Result<bool, ViewContainerError> {
        let view = self.view.read().await;
        Ok(view.contains_key(key).await?)
    }

    async fn contains_keys(&self, keys: Vec<Vec<u8>>) -> Result<Vec<bool>, ViewContainerError> {
        let view = self.view.read().await;
        Ok(view.contains_keys(keys).await?)
    }

    async fn read_multi_values_bytes(
        &self,
        keys: Vec<Vec<u8>>,
    ) -> Result<Vec<Option<Vec<u8>>>, ViewContainerError> {
        let view = self.view.read().await;
        Ok(view.multi_get(keys).await?)
    }

    async fn find_keys_by_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<Vec<Vec<u8>>, ViewContainerError> {
        let view = self.view.read().await;
        Ok(view.find_keys_by_prefix(key_prefix).await?)
    }

    async fn find_key_values_by_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ViewContainerError> {
        let view = self.view.read().await;
        Ok(view.find_key_values_by_prefix(key_prefix).await?)
    }
}

#[cfg(with_testing)]
impl<C: Context> WritableKeyValueStore for ViewContainer<C> {
    const MAX_VALUE_SIZE: usize = <C::Store as WritableKeyValueStore>::MAX_VALUE_SIZE;

    async fn write_batch(&self, batch: Batch) -> Result<(), ViewContainerError> {
        let mut view = self.view.write().await;
        view.write_batch(batch).await?;
        let mut batch = Batch::new();
        view.flush(&mut batch)?;
        view.context()
            .store()
            .write_batch(batch)
            .await
            .map_err(ViewError::from)?;
        Ok(())
    }

    async fn clear_journal(&self) -> Result<(), ViewContainerError> {
        Ok(())
    }
}

#[cfg(with_testing)]
impl<C: Context> ViewContainer<C> {
    /// Creates a [`ViewContainer`].
    pub async fn new(context: C) -> Result<Self, ViewError> {
        let view = KeyValueStoreView::load(context).await?;
        let view = Arc::new(RwLock::new(view));
        Ok(Self { view })
    }
}
