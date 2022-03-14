mod common;

use phaselock::{
    demos::dentry::{random_transaction, DEntryBlock, State},
    traits::{BlockContents, Storage},
    H_256,
};
use rand::thread_rng;

type AtomicStorage = phaselock::traits::implementations::AtomicStorage<DEntryBlock, State, H_256>;

#[async_std::test]
async fn test_happy_path_blocks() {
    // This folder will be destroyed when the last handle to it closes
    let file = tempfile::tempdir().expect("Could not create temp dir");
    let path = file.path();
    println!("Using store in {:?}", path);
    let store = AtomicStorage::open(path).expect("Could not open atomic store");
    assert_eq!(store.uncommitted_change_count().await, 0);

    let block = DEntryBlock::default();
    let hash = block.hash();
    store.insert_block(hash, block.clone()).await;
    assert_ne!(store.uncommitted_change_count().await, 0);
    store.commit().await.expect("Could not commit");
    assert_eq!(store.uncommitted_change_count().await, 0);

    // Make sure the data is still there after re-opening
    drop(store);
    let store = AtomicStorage::open(path).expect("Could not open atomic store");
    assert_eq!(
        store.get_block(&hash).await.unwrap(),
        DEntryBlock::default()
    );

    // Add some transactions
    let mut rng = thread_rng();
    let state = common::get_starting_state();
    let mut hashes = Vec::new();
    let mut block = block;
    for _ in 0..10 {
        let new = block
            .add_transaction_raw(&random_transaction(&state, &mut rng))
            .expect("Could not add transaction");
        println!("Inserting {:?}", new);
        store.insert_block(new.hash(), new.clone());
        hashes.push(new.hash());
        block = new;
    }
    store.commit().await.expect("Could not commit store");

    // read them all back
    for hash in hashes {
        let block = store.get_block(&hash).await.unwrap();
        println!("read {:?}", block);
    }
}
