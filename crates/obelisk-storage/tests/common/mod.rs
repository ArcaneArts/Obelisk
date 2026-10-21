use obelisk_storage::*;
use std::sync::Arc;

pub async fn contract(store: Arc<dyn Storage>) {
    let id = ObjectId::random();
    assert!(matches!(store.read(id, None).await, Err(Error::NotFound)));
    store
        .create(id, Bytes::from_static(b"abcdef"))
        .await
        .unwrap();
    assert!(matches!(
        store.create(id, Bytes::from_static(b"overwrite")).await,
        Err(Error::Conflict)
    ));
    assert_eq!(store.read(id, None).await.unwrap(), "abcdef");
    assert_eq!(store.read(id, Some(1..4)).await.unwrap(), "bcd");
    assert!(store.read(id, Some(1..7)).await.is_err());
    assert_eq!(store.read(id, Some(6..6)).await.unwrap().len(), 0);
    assert!(store.list().await.unwrap().contains(&id));
    assert!(store.read_head().await.unwrap().is_none());
    let first = store
        .compare_exchange_head(None, "first".into())
        .await
        .unwrap();
    assert!(matches!(
        store.compare_exchange_head(None, "bad".into()).await,
        Err(Error::Conflict)
    ));
    let mut tasks = vec![];
    for i in 0..8 {
        let store = store.clone();
        let first = first.clone();
        tasks.push(tokio::spawn(async move {
            store
                .compare_exchange_head(Some(&first), format!("next-{i}").into())
                .await
        }));
    }
    let mut wins = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => wins += 1,
            Err(Error::Conflict) => (),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(wins, 1);
    let current = store.read_head().await.unwrap().unwrap();
    assert_ne!(current.version, first);
    assert!(current.bytes.starts_with(b"next-"));
    store.delete(id).await.unwrap();
    store.delete(id).await.unwrap();
    assert!(matches!(store.read(id, None).await, Err(Error::NotFound)));
}
use bytes::Bytes;
