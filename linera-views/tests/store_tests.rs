// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use linera_views::{
    batch::Batch,
    context::{Context as _, MemoryContext},
    key_value_store_view::ViewContainer,
    memory::MemoryDatabase,
    random::make_deterministic_rng,
    store::{ReadableKeyValueStore as _, TestKeyValueDatabase as _, WritableKeyValueStore as _},
    test_utils::{
        big_read_multi_values, get_random_test_scenarios, run_big_write_read, run_reads,
        run_writes_from_blank, run_writes_from_state,
    },
    value_splitting::create_value_splitting_memory_store,
};
#[cfg(web)]
use wasm_bindgen_test::wasm_bindgen_test;

#[cfg(web)]
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

#[cfg(any(with_dynamodb, with_scylladb))]
use linera_views::test_utils::access_admin_test;

#[ignore]
#[tokio::test]
async fn test_read_multi_values_memory() {
    let config = MemoryDatabase::new_test_config().await.unwrap();
    big_read_multi_values::<MemoryDatabase>(config, 2200000, 1000).await;
}

#[ignore]
#[cfg(with_dynamodb)]
#[tokio::test]
async fn test_read_multi_values_dynamo_db() {
    use linera_views::dynamo_db::DynamoDbDatabase;
    let config = DynamoDbDatabase::new_test_config().await.unwrap();
    big_read_multi_values::<DynamoDbDatabase>(config, 22000000, 1000).await;
}

#[ignore]
#[cfg(with_scylladb)]
#[tokio::test]
async fn test_read_multi_values_scylla_db() {
    use linera_views::scylla_db::ScyllaDbDatabase;
    let config = ScyllaDbDatabase::new_test_config().await.unwrap();
    big_read_multi_values::<ScyllaDbDatabase>(config, 22200000, 200).await;
}

#[tokio::test]
async fn test_reads_test_memory() {
    for scenario in get_random_test_scenarios() {
        let key_value_store = create_value_splitting_memory_store();
        run_reads(key_value_store, scenario).await;
    }
}

#[tokio::test]
async fn test_reads_memory() {
    for scenario in get_random_test_scenarios() {
        let store = MemoryDatabase::new_test_store().await.unwrap();
        run_reads(store, scenario).await;
    }
}

#[cfg(with_rocksdb)]
#[tokio::test]
async fn test_reads_rocks_db() {
    for scenario in get_random_test_scenarios() {
        let store = linera_views::rocks_db::RocksDbDatabase::new_test_store()
            .await
            .unwrap();
        run_reads(store, scenario).await;
    }
}

#[cfg(with_dynamodb)]
#[tokio::test]
async fn test_reads_dynamo_db() {
    use linera_views::store::KeyValueDatabase as _;

    for scenario in get_random_test_scenarios() {
        let database = linera_views::dynamo_db::DynamoDbDatabase::connect_test_namespace()
            .await
            .unwrap();
        let store = database.open_exclusive(&[]).unwrap();
        run_reads(store, scenario).await;
    }
}

#[cfg(with_scylladb)]
#[tokio::test]
async fn test_reads_scylla_db() {
    use linera_views::store::KeyValueDatabase as _;

    for scenario in get_random_test_scenarios() {
        let database = linera_views::scylla_db::ScyllaDbDatabase::connect_test_namespace()
            .await
            .unwrap();
        let store = database.open_exclusive(&[]).unwrap();
        run_reads(store, scenario).await;
    }
}

#[cfg(with_scylladb)]
#[tokio::test]
async fn test_reads_scylla_db_no_root_key() {
    for scenario in get_random_test_scenarios() {
        let store = linera_views::scylla_db::ScyllaDbDatabase::new_test_store()
            .await
            .unwrap();
        run_reads(store, scenario).await;
    }
}

#[cfg(with_indexeddb)]
#[wasm_bindgen_test]
async fn test_reads_indexed_db() {
    for scenario in get_random_test_scenarios() {
        let key_value_store = linera_views::indexed_db::create_indexed_db_test_store().await;
        run_reads(key_value_store, scenario).await;
    }
}

#[tokio::test]
async fn test_reads_key_value_store_view_memory() {
    for scenario in get_random_test_scenarios() {
        let context = MemoryContext::new_for_testing(());
        let key_value_store = ViewContainer::new(context).await.unwrap();
        run_reads(key_value_store, scenario).await;
    }
}

#[tokio::test]
async fn test_specific_reads_memory() {
    let store = MemoryDatabase::new_test_store().await.unwrap();
    let key_values = vec![
        (vec![0, 1, 255], Vec::new()),
        (vec![0, 1, 255, 37], Vec::new()),
        (vec![0, 2], Vec::new()),
        (vec![0, 2, 0], Vec::new()),
    ];
    run_reads(store, key_values).await;
}

#[tokio::test]
async fn test_test_memory_writes_from_blank() {
    let key_value_store = create_value_splitting_memory_store();
    run_writes_from_blank(&key_value_store).await;
}

#[tokio::test]
async fn test_memory_writes_from_blank() {
    let store = MemoryDatabase::new_test_store().await.unwrap();
    run_writes_from_blank(&store).await;
}

#[tokio::test]
async fn test_key_value_store_view_memory_writes_from_blank() {
    let context = MemoryContext::new_for_testing(());
    let key_value_store = ViewContainer::new(context).await.unwrap();
    run_writes_from_blank(&key_value_store).await;
}

#[cfg(with_rocksdb)]
#[tokio::test]
async fn test_rocks_db_writes_from_blank() {
    let store = linera_views::rocks_db::RocksDbDatabase::new_test_store()
        .await
        .unwrap();
    run_writes_from_blank(&store).await;
}

#[cfg(with_dynamodb)]
#[tokio::test]
async fn test_dynamo_db_writes_from_blank() {
    let store = linera_views::dynamo_db::DynamoDbDatabase::new_test_store()
        .await
        .unwrap();
    run_writes_from_blank(&store).await;
}

#[cfg(with_scylladb)]
#[tokio::test]
async fn test_scylla_db_writes_from_blank() {
    let store = linera_views::scylla_db::ScyllaDbDatabase::new_test_store()
        .await
        .unwrap();
    run_writes_from_blank(&store).await;
}

#[cfg(with_indexeddb)]
#[wasm_bindgen_test]
async fn test_indexed_db_writes_from_blank() {
    let key_value_store = linera_views::indexed_db::create_indexed_db_test_store().await;
    run_writes_from_blank(&key_value_store).await;
}

#[tokio::test]
async fn test_big_value_read_write() {
    use rand::{distributions::Alphanumeric, Rng};
    let context = MemoryContext::new_for_testing(());
    for count in [50, 1024] {
        let rng = make_deterministic_rng();
        let test_string = rng
            .sample_iter(&Alphanumeric)
            .take(count)
            .map(char::from)
            .collect::<String>();
        let mut batch = Batch::new();
        let key = vec![43, 23, 56];
        batch.put_key_value(key.clone(), &test_string).unwrap();
        context.store().write_batch(batch).await.unwrap();
        let read_string = context
            .store()
            .read_value::<String>(&key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read_string, test_string);
    }
}

#[cfg(with_scylladb)]
#[tokio::test]
async fn scylla_db_tombstone_triggering_test() {
    use linera_views::store::KeyValueDatabase as _;

    let database = linera_views::scylla_db::ScyllaDbDatabase::connect_test_namespace()
        .await
        .unwrap();
    let store = database.open_exclusive(&[]).unwrap();
    linera_views::test_utils::tombstone_triggering_test(store).await;
}

#[cfg(with_rocksdb)]
#[tokio::test]
async fn rocks_db_tombstone_triggering_test() {
    let store = linera_views::rocks_db::RocksDbDatabase::new_test_store()
        .await
        .unwrap();
    linera_views::test_utils::tombstone_triggering_test(store).await;
}

#[cfg(with_scylladb)]
#[tokio::test]
async fn test_scylla_db_big_write_read() {
    use linera_views::store::KeyValueDatabase as _;

    let database = linera_views::scylla_db::ScyllaDbDatabase::connect_test_namespace()
        .await
        .unwrap();
    let store = database.open_exclusive(&[]).unwrap();
    let value_sizes = vec![100, 1000, 200000, 5000000];
    let target_size = 20000000;
    run_big_write_read(store, target_size, value_sizes).await;
}

#[tokio::test]
async fn test_memory_big_write_read() {
    let store = MemoryDatabase::new_test_store().await.unwrap();
    let value_sizes = vec![100, 1000, 200000, 5000000];
    let target_size = 20000000;
    run_big_write_read(store, target_size, value_sizes).await;
}

#[cfg(with_rocksdb)]
#[tokio::test]
async fn test_rocks_db_big_write_read() {
    let store = linera_views::rocks_db::RocksDbDatabase::new_test_store()
        .await
        .unwrap();
    let value_sizes = vec![100, 1000, 200000, 5000000];
    let target_size = 20000000;
    run_big_write_read(store, target_size, value_sizes).await;
}

#[cfg(with_indexeddb)]
#[wasm_bindgen_test]
async fn test_indexed_db_big_write_read() {
    let key_value_store = linera_views::indexed_db::create_indexed_db_test_store().await;
    let value_sizes = vec![100, 1000, 200000, 5000000];
    let target_size = 20000000;
    run_big_write_read(key_value_store, target_size, value_sizes).await;
}

#[cfg(with_dynamodb)]
#[tokio::test]
async fn test_dynamo_db_big_write_read() {
    use linera_views::store::KeyValueDatabase as _;

    let database = linera_views::dynamo_db::DynamoDbDatabase::connect_test_namespace()
        .await
        .unwrap();
    let store = database.open_exclusive(&[]).unwrap();
    let value_sizes = vec![100, 1000, 200000, 5000000];
    let target_size = 20000000;
    run_big_write_read(store, target_size, value_sizes).await;
}

#[tokio::test]
async fn test_memory_writes_from_state() {
    let store = MemoryDatabase::new_test_store().await.unwrap();
    run_writes_from_state(&store).await;
}

#[cfg(with_rocksdb)]
#[tokio::test]
async fn test_rocks_db_writes_from_state() {
    let store = linera_views::rocks_db::RocksDbDatabase::new_test_store()
        .await
        .unwrap();
    run_writes_from_state(&store).await;
}

#[cfg(with_indexeddb)]
#[wasm_bindgen_test]
async fn test_indexed_db_writes_from_state() {
    let key_value_store = linera_views::indexed_db::create_indexed_db_test_store().await;
    run_writes_from_state(&key_value_store).await;
}

#[cfg(with_dynamodb)]
#[tokio::test]
async fn test_dynamo_db_writes_from_state() {
    let store = linera_views::dynamo_db::DynamoDbDatabase::new_test_store()
        .await
        .unwrap();
    run_writes_from_state(&store).await;
}

#[cfg(with_scylladb)]
#[tokio::test]
async fn test_scylla_db_writes_from_state() {
    let store = linera_views::scylla_db::ScyllaDbDatabase::new_test_store()
        .await
        .unwrap();
    run_writes_from_state(&store).await;
}

#[cfg(with_scylladb)]
#[tokio::test]
async fn test_scylladb_access() {
    access_admin_test::<linera_views::scylla_db::ScyllaDbDatabase>().await
}

#[cfg(with_dynamodb)]
#[tokio::test]
async fn test_dynamodb_access() {
    access_admin_test::<linera_views::dynamo_db::DynamoDbDatabase>().await
}
