//! Logical scaling measurements; no hardware-independent latency thresholds.
use obelisk::{
    diagnostics::{InstrumentedMetadata, ObjectKind},
    storage::{
        Storage,
        metrics::{InstrumentedStorage, Stats},
    },
    *,
};
use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
type AnyResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[cfg(feature = "folder")]
async fn folder(path: PathBuf) -> AnyResult<Arc<dyn Storage>> {
    Ok(Arc::new(storage::folder::Folder::open(path).await?))
}
#[cfg(not(feature = "folder"))]
async fn folder(_path: PathBuf) -> AnyResult<Arc<dyn Storage>> {
    Err("enable folder or choose s3".into())
}
#[cfg(feature = "s3")]
fn s3(bucket: &str, prefix: &str) -> AnyResult<Arc<dyn Storage>> {
    Ok(Arc::new(storage::s3::S3::from_env(bucket, prefix)?))
}
#[cfg(not(feature = "s3"))]
fn s3(_bucket: &str, _prefix: &str) -> AnyResult<Arc<dyn Storage>> {
    Err("enable s3".into())
}
async fn backend(
    root: &Path,
    cloud: Option<&(String, String)>,
    profile: &str,
) -> AnyResult<Arc<dyn Storage>> {
    if let Some((bucket, prefix)) = cloud {
        s3(bucket, &format!("{prefix}/{profile}"))
    } else {
        folder(root.join(profile)).await
    }
}
const KINDS: [ObjectKind; 8] = [
    ObjectKind::Root,
    ObjectKind::Documents,
    ObjectKind::Requests,
    ObjectKind::Membership,
    ObjectKind::Outcome,
    ObjectKind::Record,
    ObjectKind::Segment,
    ObjectKind::Unknown,
];
fn charges(d: &InstrumentedMetadata) -> u64 {
    KINDS.iter().map(|k| d.stats(*k).reader_decode_charge).sum()
}
fn pages(d: &InstrumentedMetadata) -> u64 {
    [
        ObjectKind::Documents,
        ObjectKind::Requests,
        ObjectKind::Membership,
    ]
    .iter()
    .map(|k| d.stats(*k).reads)
    .sum()
}
async fn measured<T>(
    name: &str,
    size: usize,
    metrics: &InstrumentedStorage,
    diag: &InstrumentedMetadata,
    work: impl Future<Output = obelisk::Result<T>>,
) -> obelisk::Result<T> {
    let before = metrics.stats();
    let payload = diag.stats(ObjectKind::Record).bytes_read;
    let charge = charges(diag);
    let page_reads = pages(diag);
    let started = Instant::now();
    let value = work.await?;
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    let after = metrics.stats();
    let payload_bytes = diag.stats(ObjectKind::Record).bytes_read - payload;
    println!(
        "| {name} | {size} | {elapsed:.3} | {} | {} | {payload_bytes} | {} | {} | {} | {} | {} | {} |",
        after.object_reads - before.object_reads,
        after.bytes_read - before.bytes_read - payload_bytes,
        pages(diag) - page_reads,
        charges(diag) - charge,
        after.object_creates - before.object_creates,
        after.bytes_written - before.bytes_written,
        after.head_cas_attempts - before.head_cas_attempts,
        after.head_conflicts - before.head_conflicts
    );
    Ok(value)
}
fn distribution(name: &str, size: usize, samples: &[f64]) {
    let mut samples = samples.to_vec();
    samples.sort_by(f64::total_cmp);
    let at = |p: f64| samples[((samples.len() as f64 * p).ceil() as usize).saturating_sub(1)];
    println!(
        "latency {name}: size={size}, samples={}, p50={:.3} ms, p95={:.3} ms, p99={:.3} ms",
        samples.len(),
        at(0.5),
        at(0.95),
        at(0.99)
    );
}
async fn latencies<T, F, Fut>(name: &str, size: usize, mut work: F) -> obelisk::Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = obelisk::Result<T>>,
{
    let mut samples = Vec::new();
    for _ in 0..8 {
        let start = Instant::now();
        work().await?;
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    distribution(name, size, &samples);
    Ok(())
}
async fn startup(
    name: &str,
    size: usize,
    metrics: Arc<InstrumentedStorage>,
    diag: &InstrumentedMetadata,
) -> obelisk::Result<()> {
    let roots = diag.stats(ObjectKind::Root).reads;
    let before_pages = pages(diag);
    let payload = diag.stats(ObjectKind::Record).reads;
    let outcomes = diag.stats(ObjectKind::Outcome).reads;
    measured(
        name,
        size,
        metrics.as_ref(),
        diag,
        Database::open_with_storage(metrics.clone(), DatabaseOptions::default()),
    )
    .await?;
    assert_eq!(diag.stats(ObjectKind::Root).reads - roots, 1);
    assert_eq!(pages(diag), before_pages);
    assert_eq!(diag.stats(ObjectKind::Record).reads, payload);
    assert_eq!(diag.stats(ObjectKind::Outcome).reads, outcomes);
    Ok(())
}
fn retained(root: &Path, profile: &str) -> AnyResult<()> {
    let mut count = 0;
    let mut bytes = 0;
    for file in std::fs::read_dir(root.join(profile).join("objects"))? {
        count += 1;
        bytes += file?.metadata()?.len();
    }
    println!(
        "Retained {profile}: {count} immutable objects, {bytes} bytes (including history/orphan preparations)."
    );
    Ok(())
}
fn summary(profile: &str, stats: Stats) {
    println!("{profile} logical totals, not physical HTTP requests: {stats:?}");
}
#[tokio::main]
async fn main() -> AnyResult<()> {
    let max = std::env::var("OBELISK_SCALING_MAX")
        .unwrap_or_else(|_| "100000".into())
        .parse::<usize>()?;
    let requests = std::env::var("OBELISK_SCALING_REQUESTS")
        .unwrap_or_else(|_| "10000".into())
        .parse::<usize>()?;
    if !(100..=100_000).contains(&max) || !(100..=100_000).contains(&requests) {
        return Err("scaling limits must be 100..=100000".into());
    }
    let dir = tempfile::tempdir()?;
    let cloud = match std::env::var("OBELISK_BASELINE_BACKEND").as_deref() {
        Ok("s3") => {
            let bucket = std::env::var("OBELISK_S3_BUCKET")?;
            let base = std::env::var("OBELISK_S3_TEST_PREFIX")?;
            let prefix = format!("{base}/scaling-{}", uuid::Uuid::new_v4());
            println!("S3 scaling prefix retained: {prefix}");
            Some((bucket, prefix))
        }
        Ok("folder") | Err(_) => None,
        Ok(_) => return Err("unknown backend".into()),
    };
    println!(
        "Scaling: 1024-byte binary payload, batch <=256, no engine cache; live document sizes then one-document request histories. OS caches are not flushed."
    );
    println!(
        "| Operation | Size | ms | Object reads | Metadata bytes incl HEAD | Payload bytes | Tree pages read | Reader decode charge* | Objects created | Bytes written incl HEAD | CAS | Conflicts |"
    );
    println!(
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    let diag = Arc::new(InstrumentedMetadata::new(
        backend(dir.path(), cloud.as_ref(), "data").await?,
    ));
    let metrics = Arc::new(InstrumentedStorage::new(diag.clone()));
    let db = Database::open_with_storage(metrics.clone(), DatabaseOptions::default()).await?;
    let document = Document::new([(FieldPath::parse("payload")?, Value::Binary(vec![7; 1024]))])?;
    let mut written = 0;
    for target in [100, 1000, 10000, 100000] {
        if target > max {
            continue;
        }
        let mut last_request = String::new();
        let mut fill_samples = Vec::new();
        while written < target {
            let end = (written + 256).min(target);
            last_request = format!("fill-{written}");
            let mutations = (written..end)
                .map(|i| {
                    Ok(Mutation {
                        path: DocumentPath::parse(&format!("docs/{i:08}"))?,
                        condition: Condition::Any,
                        kind: MutationKind::Put(document.clone()),
                    })
                })
                .collect::<std::result::Result<Vec<_>, ValidationError>>()?;
            let fill_started = Instant::now();
            db.commit(&Transaction::new(
                RequestId::new(last_request.clone())?,
                mutations,
                vec![],
            )?)
            .await?;
            fill_samples.push(fill_started.elapsed().as_secs_f64() * 1000.0);
            if end / 10_000 > written / 10_000 {
                eprintln!("live fill progress: {end} documents");
            }
            written = end;
        }
        startup("open-live", target, metrics.clone(), diag.as_ref()).await?;
        measured(
            "point",
            target,
            metrics.as_ref(),
            diag.as_ref(),
            db.get("docs/00000000"),
        )
        .await?;
        let keys: Vec<_> = (0..100).map(|i| format!("docs/{i:08}")).collect();
        let refs: Vec<_> = keys.iter().map(String::as_str).collect();
        assert_eq!(
            measured(
                "bulk-100",
                target,
                metrics.as_ref(),
                diag.as_ref(),
                db.get_many(&refs)
            )
            .await?
            .len(),
            100
        );
        let scope = QueryScope::path("docs")?;
        assert_eq!(
            measured(
                "children-100",
                target,
                metrics.as_ref(),
                diag.as_ref(),
                db.children(&scope, PageOptions::default(), None)
            )
            .await?
            .entries
            .len(),
            100
        );
        assert_eq!(
            measured(
                "descendants-100",
                target,
                metrics.as_ref(),
                diag.as_ref(),
                db.descendants(&scope, PageOptions::default(), None)
            )
            .await?
            .entries
            .len(),
            100
        );
        assert!(
            measured(
                "resolve",
                target,
                metrics.as_ref(),
                diag.as_ref(),
                db.resolve(&last_request)
            )
            .await?
            .is_some()
        );
        let before = diag.stats(ObjectKind::Membership).creates;
        measured(
            "update-one",
            target,
            metrics.as_ref(),
            diag.as_ref(),
            db.put(
                &format!("probe-{target}"),
                "docs/00000000",
                document.clone(),
                Condition::Any,
            ),
        )
        .await?;
        assert_eq!(diag.stats(ObjectKind::Membership).creates, before);
        distribution("fill-batch", target, &fill_samples);
        latencies("open-live", target, || {
            Database::open_with_storage(metrics.clone(), DatabaseOptions::default())
        })
        .await?;
        latencies("point", target, || db.get("docs/00000000")).await?;
        latencies("children-100", target, || {
            db.children(&scope, PageOptions::default(), None)
        })
        .await?;
        latencies("descendants-100", target, || {
            db.descendants(&scope, PageOptions::default(), None)
        })
        .await?;
        eprintln!("live scaling checkpoint: {target} documents");
    }
    summary("live", metrics.stats());
    if cloud.is_none() {
        retained(dir.path(), "data")?;
    }
    let diag = Arc::new(InstrumentedMetadata::new(
        backend(dir.path(), cloud.as_ref(), "hot").await?,
    ));
    let metrics = Arc::new(InstrumentedStorage::new(diag.clone()));
    let db = Database::open_with_storage(metrics.clone(), DatabaseOptions::default()).await?;
    let mut write_window = std::collections::VecDeque::new();
    for i in 1..=requests {
        let write_started = Instant::now();
        let request = format!("hot-{i}");
        let write = db.put(&request, "hot", document.clone(), Condition::Any);
        if [100, 1000, 10000, 100000].contains(&i) {
            measured("hot-write", i, metrics.as_ref(), diag.as_ref(), write).await?;
        } else {
            write.await?;
        }
        write_window.push_back(write_started.elapsed().as_secs_f64() * 1000.0);
        if write_window.len() > 32 {
            write_window.pop_front();
        }
        if [100, 1000, 10000, 100000].contains(&i) {
            startup("open-hot-history", i, metrics.clone(), diag.as_ref()).await?;
            measured(
                "hot-point",
                i,
                metrics.as_ref(),
                diag.as_ref(),
                db.get("hot"),
            )
            .await?;
            assert!(
                measured(
                    "hot-resolve",
                    i,
                    metrics.as_ref(),
                    diag.as_ref(),
                    db.resolve("hot-1")
                )
                .await?
                .is_some()
            );
            distribution(
                "hot-write-window",
                i,
                &write_window.iter().copied().collect::<Vec<_>>(),
            );
            latencies("hot-point", i, || db.get("hot")).await?;
            latencies("hot-resolve", i, || db.resolve("hot-1")).await?;
        }
        if i.is_multiple_of(100) {
            eprintln!("hot scaling progress: {i} retained requests");
        }
    }
    summary("hot", metrics.stats());
    if cloud.is_none() {
        retained(dir.path(), "hot")?;
    }
    println!(
        "* Reader decode charge is the bounded root/page reader estimate, excluding editor/result copies, payload parsing, and allocator RSS. Timings are individual samples; read probes and fill batches also report sampled distributions; the baseline supplies contention distributions. Serial probes have no competing writers; use the baseline and subprocess harness for contention. Actual S3 needs separate service-side HTTP/retry metrics."
    );
    Ok(())
}
