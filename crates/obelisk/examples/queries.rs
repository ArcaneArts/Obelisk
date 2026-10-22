//! Run with a fresh POSIX folder: cargo run -p obelisk --example queries -- /tmp/obelisk-queries
use obelisk::*;
#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let folder = std::env::args()
        .nth(1)
        .ok_or("provide a v2 database folder")?;
    let db = Database::open_folder(folder).await?;
    for i in 0..5 {
        db.put(
            &format!("query-example-{i}"),
            &format!("projects/{i}"),
            Document::default(),
            Condition::Any,
        )
        .await?;
    }
    let bulk = db
        .get_many(&["projects/0", "missing", "projects/0"])
        .await?;
    println!(
        "Bulk reads preserve duplicates and missing entries: {} inputs",
        bulk.len()
    );
    let scope = QueryScope::path("projects")?;
    let options = PageOptions::new(2)?;
    let mut cursor = None;
    loop {
        let page = db.children(&scope, options, cursor.as_ref()).await?;
        for entry in page.entries {
            println!("{} @ {}", entry.path, entry.revision);
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let roots = db
        .children(&QueryScope::Root, PageOptions::default(), None)
        .await?;
    println!(
        "{} existing root documents; missing ancestors are not synthesized",
        roots.entries.len()
    );
    Ok(())
}
