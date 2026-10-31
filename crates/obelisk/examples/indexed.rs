//! Run against a fresh folder, or an explicitly configured fresh S3 prefix.
use obelisk::*;
use std::{error::Error as StdError, ops::Bound};
type ExampleResult<T> = std::result::Result<T, Box<dyn StdError>>;
fn field(text: &str) -> FieldPath {
    FieldPath::parse(text).unwrap()
}
async fn open() -> ExampleResult<Database> {
    match std::env::var("OBELISK_EXAMPLE_BACKEND")
        .as_deref()
        .unwrap_or("folder")
    {
        "folder" => {
            #[cfg(feature = "folder")]
            {
                Ok(Database::open_folder(
                    std::env::args()
                        .nth(1)
                        .unwrap_or_else(|| "obelisk-indexed-v3".into()),
                )
                .await?)
            }
            #[cfg(not(feature = "folder"))]
            {
                Err("build with the folder feature".into())
            }
        }
        "s3" => {
            #[cfg(feature = "s3")]
            {
                Ok(Database::open_s3(
                    &std::env::var("OBELISK_S3_BUCKET")?,
                    &std::env::var("OBELISK_S3_TEST_PREFIX")?,
                )
                .await?)
            }
            #[cfg(not(feature = "s3"))]
            {
                Err("build with the s3 feature".into())
            }
        }
        _ => Err("OBELISK_EXAMPLE_BACKEND must be folder or s3".into()),
    }
}
#[tokio::main]
async fn main() -> ExampleResult<()> {
    let db = open().await?;
    let mut mutations = Vec::new();
    for (name, score) in [("Ada", 3), ("Gus", 2), ("Lin", 1)] {
        mutations.push(Mutation {
            path: DocumentPath::from_segments(["people", name])?,
            condition: Condition::Any,
            kind: MutationKind::Put(Document::new([
                (field("kind"), Value::String("open".into())),
                (field("score"), Value::Integer(score)),
                (field("name"), Value::String(name.into())),
            ])?),
        });
    }
    // This application request replays its original receipt on subsequent runs.
    let seeded = db
        .commit(&Transaction::new(
            RequestId::new("indexed-example-seed")?,
            mutations,
            vec![],
        )?)
        .await?;
    println!(
        "seed commit {} sequence {}",
        seeded.commit_id, seeded.sequence
    );
    // A fresh operation namespace lets the example run again after its final drop.
    let run = uuid::Uuid::new_v4().to_string();
    let create_request = format!("{run}-create");
    let definition = IndexDefinition::new(
        IndexId::new("by-kind-score")?,
        IndexScope::Descendants(QueryScope::path("people")?),
        vec![
            IndexField::new(field("kind"), ScalarType::String, IndexOrder::Ascending),
            IndexField::new(field("score"), ScalarType::Integer, IndexOrder::Descending),
        ],
        vec![field("name")],
    )?;
    let created = match db.create_index(&create_request, definition.clone()).await {
        Ok(receipt) => receipt,
        Err(Error::Uncertain { .. }) => match db.resolve_index_operation(&create_request).await? {
            Some(receipt) => receipt,
            None => db.create_index(&create_request, definition.clone()).await?,
        },
        Err(error) => return Err(error.into()),
    };
    db.advance_index_build(
        &format!("{run}-first"),
        &created.handle,
        IndexBuildOptions::new(1)?,
    )
    .await?;
    drop(db);
    let db = open().await?;
    let resumed = db
        .get_index(definition.id())
        .await?
        .ok_or("index missing after reopen")?;
    let progress = resumed
        .candidate
        .as_ref()
        .ok_or("candidate missing")?
        .progress
        .examined();
    println!("resumed after {progress} examined document(s)");
    for step in 0..100 {
        let receipt = db
            .advance_index_build(
                &format!("{run}-step-{step}"),
                &created.handle,
                IndexBuildOptions::new(1)?,
            )
            .await?;
        if receipt.state == IndexState::Active {
            break;
        }
        if receipt.state == IndexState::Failed {
            return Err(format!("build failed: {:?}", receipt.failure).into());
        }
    }
    assert_eq!(
        db.resolve_index_operation(&create_request).await?,
        Some(created.clone())
    );
    assert_eq!(
        db.create_index(&create_request, definition.clone()).await?,
        created
    );
    let query = IndexedQuery::new(
        definition.scope().clone(),
        vec![EqualityPredicate::new(
            field("kind"),
            Value::String("open".into()),
        )?],
        Some(ScalarRange::new(
            Bound::Included(Value::Integer(1)),
            Bound::Unbounded,
        )?),
        definition.fields()[1..].to_vec(),
        Projection::fields(vec![field("score"), field("name")])?,
    )?;
    let first = db.query(&query, PageOptions::new(1)?, None).await?;
    let pinned = db.snapshot_at(first.snapshot.clone()).await?;
    let rebuild = db
        .rebuild_index(&format!("{run}-rebuild"), &created.handle)
        .await?;
    let activated = db
        .advance_index_build(
            &format!("{run}-activate"),
            &rebuild.handle,
            IndexBuildOptions::new(256)?,
        )
        .await?;
    assert_eq!(activated.state, IndexState::Active);
    db.drop_index(&format!("{run}-drop"), &rebuild.handle)
        .await?;
    // The retained continuation still reads its original generation after drop.
    let mut entries = first.entries;
    let mut cursor = first.next;
    while let Some(position) = cursor {
        let page = db
            .query(&query, PageOptions::new(2)?, Some(&position))
            .await?;
        entries.extend(page.entries);
        cursor = page.next;
    }
    assert_eq!(
        entries,
        pinned
            .query(&query, PageOptions::default(), None)
            .await?
            .entries
    );
    for entry in entries {
        println!("{} {:?}", entry.path, entry.document.fields());
    }
    Ok(())
}
