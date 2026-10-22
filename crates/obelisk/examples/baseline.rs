//! Repeatable local workload, without hard-coded performance pass/fail targets.
use obelisk::{storage::metrics::InstrumentedStorage, *};
use std::{sync::Arc, time::Instant};
#[path = "support/publications.rs"]
mod publications;
fn report(name: &str, samples: &mut [f64]) {
    samples.sort_by(f64::total_cmp);
    let percentile =
        |p: f64| samples[((samples.len() as f64 * p).ceil() as usize).saturating_sub(1)];
    println!(
        "| {name} | {} | {:.3} | {:.3} | {:.3} |",
        samples.len(),
        percentile(0.5),
        percentile(0.95),
        percentile(0.99)
    );
}
#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let backend: Arc<dyn storage::Storage> =
        match std::env::var("OBELISK_BASELINE_BACKEND").as_deref() {
            Ok("s3") => {
                #[cfg(feature = "s3")]
                {
                    let bucket = std::env::var("OBELISK_S3_BUCKET")?;
                    let base = std::env::var("OBELISK_S3_TEST_PREFIX")?;
                    let prefix = format!("{base}/baseline-{}", uuid::Uuid::new_v4());
                    println!("S3 baseline prefix retained: {prefix}");
                    Arc::new(storage::s3::S3::from_env(&bucket, &prefix)?)
                }
                #[cfg(not(feature = "s3"))]
                {
                    return Err("enable the s3 feature".into());
                }
            }
            Ok("folder") | Err(_) => {
                #[cfg(feature = "folder")]
                {
                    Arc::new(storage::folder::Folder::open(dir.path()).await?)
                }
                #[cfg(not(feature = "folder"))]
                {
                    return Err("enable folder or select the s3 backend".into());
                }
            }
            Ok(_) => return Err("unknown OBELISK_BASELINE_BACKEND".into()),
        };
    let is_folder = std::env::var("OBELISK_BASELINE_BACKEND").as_deref() != Ok("s3");
    let measured = Arc::new(InstrumentedStorage::new(backend.clone()));
    let db = Database::open_with_storage(measured.clone(), DatabaseOptions::default()).await?;
    let document = Document::new([(FieldPath::parse("payload")?, Value::Binary(vec![7; 1024]))])?;
    let mut writes = Vec::new();
    for i in 0..50 {
        let start = Instant::now();
        db.put(
            &format!("seq-{i}"),
            &format!("docs/{i}"),
            document.clone(),
            Condition::Any,
        )
        .await?;
        writes.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let start = Instant::now();
    assert!(db.get("docs/0").await?.is_some());
    let first_read = start.elapsed().as_secs_f64() * 1000.0;
    let mut reads = Vec::new();
    for _ in 0..100 {
        let start = Instant::now();
        assert!(db.get("docs/0").await?.is_some());
        reads.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let mut reopens = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        let reopened =
            Database::open_with_storage(measured.clone(), DatabaseOptions::default()).await?;
        assert_eq!(reopened.snapshot().await?.sequence(), 50);
        reopens.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let before = measured.stats();
    let mut tasks = Vec::new();
    let mut publications = Vec::new();
    for writer in 0..8 {
        let accounting = Arc::new(publications::Publications::new(measured.clone()));
        publications.push(accounting.clone());
        let document = document.clone();
        tasks.push(tokio::spawn(async move {
            let db = Database::open_with_storage(accounting, DatabaseOptions::default()).await?;
            let mut samples = Vec::new();
            for i in 0..5 {
                let start = Instant::now();
                db.put(
                    &format!("concurrent-{writer}-{i}"),
                    &format!("concurrent/{writer}/{i}"),
                    document.clone(),
                    Condition::Any,
                )
                .await?;
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            Ok::<_, obelisk::Error>(samples)
        }));
    }
    let mut concurrent = Vec::new();
    for task in tasks {
        concurrent.extend(task.await??);
    }
    let after = measured.stats();
    let mut accounting = publications::Accounting::default();
    for writer in publications {
        accounting.add(writer.stats());
    }
    assert_eq!(accounting.committed.attempts, 40);
    assert_eq!(
        accounting.abandoned.attempts,
        after.head_conflicts - before.head_conflicts
    );
    assert_eq!(accounting.unclassified.attempts, 0);
    assert_eq!(accounting.pending.objects, 0);
    assert_eq!(
        accounting.committed.objects + accounting.abandoned.objects,
        after.object_creates - before.object_creates
    );
    let successful_head_bytes = after.bytes_written
        - before.bytes_written
        - accounting.committed.bytes
        - accounting.abandoned.bytes;
    println!(
        "Baseline: 1024-byte binary payload; 50 sequential writes, 100 latest reads, 20 reopen+snapshot operations, 8 writers x 5 writes."
    );
    println!(
        "OS: {} / {}; first measured read: {first_read:.3} ms. OS filesystem caches are not flushed.",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "| Workload | Samples | p50 ms | p95 ms | p99 ms |\n| --- | ---: | ---: | ---: | ---: |"
    );
    report("Sequential durable write", &mut writes);
    report("Repeated latest read", &mut reads);
    report("Reopen + snapshot", &mut reopens);
    report("Concurrent durable write", &mut concurrent);
    println!(
        "Concurrent CAS attempts: {}; conflicts: {}; acknowledged commits: {}.",
        after.head_cas_attempts - before.head_cas_attempts,
        after.head_conflicts - before.head_conflicts,
        after.head_cas_successes - before.head_cas_successes
    );
    println!(
        "Concurrent immutable preparation uploads by CAS outcome: {accounting:?}; successful HEAD bytes: {successful_head_bytes}. Non-success/non-conflict results remain unclassified, never inferred committed."
    );
    let snapshot = db.snapshot().await?;
    assert_eq!(snapshot.sequence(), 90);
    let startup = Arc::new(InstrumentedStorage::new(backend));
    let reopened = Database::open_with_storage(startup.clone(), DatabaseOptions::default()).await?;
    println!(
        "Opening populated database (logical adapter operations): {:?}",
        startup.stats()
    );
    assert_eq!(reopened.snapshot().await?.sequence(), 90);
    let mut objects = 0;
    let mut bytes = 0;
    if is_folder {
        for entry in std::fs::read_dir(dir.path().join("objects"))? {
            objects += 1;
            bytes += entry?.metadata()?.len();
        }
    }
    if is_folder {
        println!("Stored objects including retained/orphaned versions: {objects}; bytes: {bytes}.");
    }
    println!(
        "Total logical adapter operations (not physical HTTP requests): {:?}",
        measured.stats()
    );
    Ok(())
}
