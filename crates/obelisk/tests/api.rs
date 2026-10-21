#![cfg(all(feature = "folder", unix))]
use obelisk::*;
#[tokio::test]
async fn public_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_folder(dir.path()).await.unwrap();
    let field = FieldPath::parse("profile.name").unwrap();
    let document = Document::new([(field.clone(), Value::String("Ada".into()))]).unwrap();
    let receipt = db.create("create", "users/ada", document).await.unwrap();
    let pinned = db.snapshot().await.unwrap();
    let revision = receipt.revisions[&DocumentPath::parse("users/ada").unwrap()]
        .clone()
        .unwrap();
    let patch = Patch::new([(field.clone(), Value::String("Augusta".into()))], vec![]).unwrap();
    db.patch("patch", "users/ada", patch, Condition::Revision(revision))
        .await
        .unwrap();
    assert_eq!(
        db.get("users/ada")
            .await
            .unwrap()
            .unwrap()
            .document
            .get(&field),
        Some(&Value::String("Augusta".into()))
    );
    assert_eq!(
        db.snapshot_at(pinned.reference())
            .await
            .unwrap()
            .get(&DocumentPath::parse("users/ada").unwrap())
            .await
            .unwrap()
            .unwrap()
            .document
            .get(&field),
        Some(&Value::String("Ada".into()))
    );
    db.delete("delete", "users/ada", Condition::Any)
        .await
        .unwrap();
    let reopened = Database::open_folder(dir.path()).await.unwrap();
    assert!(reopened.get("users/ada").await.unwrap().is_none());
    assert_eq!(reopened.resolve("create").await.unwrap(), Some(receipt));
}
