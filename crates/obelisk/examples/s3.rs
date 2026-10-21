use obelisk::{Condition, Database, Document, FieldPath, Value};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bucket = std::env::var("OBELISK_S3_BUCKET")?;
    let prefix = std::env::var("OBELISK_S3_PREFIX")?;
    let db = Database::open_s3(&bucket, &prefix).await?;
    let document = Document::new([(
        FieldPath::parse("project.name")?,
        Value::String("Obelisk".into()),
    )])?;
    let request_id = format!("example-{}", uuid::Uuid::new_v4());
    // Keep this identity and exact payload if the response is lost.
    println!("request {request_id}");
    let receipt = db
        .put(&request_id, "projects/obelisk", document, Condition::Any)
        .await?;
    println!("commit {} sequence {}", receipt.commit_id, receipt.sequence);
    println!("{:?}", db.get("projects/obelisk").await?);
    Ok(())
}
