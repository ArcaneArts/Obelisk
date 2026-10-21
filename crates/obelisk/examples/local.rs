use obelisk::{Condition, Database, Document, FieldPath, Value};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "obelisk-data".into());
    let db = Database::open_folder(path).await?;
    let document = Document::new([(
        FieldPath::parse("project.name")?,
        Value::String("Obelisk".into()),
    )])?;
    // Repeating this exact request returns its original receipt.
    let receipt = db
        .put(
            "local-example-initial",
            "projects/obelisk",
            document,
            Condition::Any,
        )
        .await?;
    println!("commit {} sequence {}", receipt.commit_id, receipt.sequence);
    println!("{:?}", db.get("projects/obelisk").await?);
    Ok(())
}
