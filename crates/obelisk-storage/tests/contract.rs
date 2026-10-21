#![cfg(all(feature = "folder", unix))]
mod common;
use obelisk_storage::{Storage, folder::Folder};
use std::sync::Arc;
#[tokio::test]
async fn folder_contract() {
    let dir = tempfile::tempdir().unwrap();
    common::contract(Arc::new(Folder::open(dir.path()).await.unwrap())).await;
}
#[tokio::test]
async fn concurrent_open_and_head_initialization() {
    let dir = tempfile::tempdir().unwrap();
    let mut tasks = vec![];
    for i in 0..16 {
        let path = dir.path().join("nested/database");
        tasks.push(tokio::spawn(async move {
            let store = Folder::open(path).await.unwrap();
            store
                .compare_exchange_head(None, format!("{i}").into())
                .await
        }));
    }
    let mut success = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => success += 1,
            Err(obelisk_storage::Error::Conflict) => (),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(success, 1);
}
