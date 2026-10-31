//! Scalar-index release qualification with structural assertions and logical IO.
use obelisk::{
    diagnostics::{InstrumentedMetadata, ObjectKind, ObjectStats},
    storage::{
        Storage,
        metrics::{InstrumentedStorage, Stats},
    },
    *,
};
use std::{collections::BTreeMap, future::Future, sync::Arc, time::Instant};
#[path = "support/publications.rs"]
mod publications;
type AnyResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const KINDS: [ObjectKind; 13] = [
    ObjectKind::Root,
    ObjectKind::Documents,
    ObjectKind::Requests,
    ObjectKind::Membership,
    ObjectKind::IndexDefinitions,
    ObjectKind::ScalarIndexes,
    ObjectKind::Outcome,
    ObjectKind::Record,
    ObjectKind::Segment,
    ObjectKind::DefinitionObject,
    ObjectKind::IndexDescriptor,
    ObjectKind::Projection,
    ObjectKind::Unknown,
];
fn field(s: &str) -> FieldPath {
    FieldPath::parse(s).unwrap()
}
fn path(n: usize) -> DocumentPath {
    DocumentPath::parse(&format!("docs/{n:06}")).unwrap()
}
fn document(n: usize, rank: i64, version: u64) -> Document {
    Document::new([
        (field("bucket"), Value::Integer((n % 1000) as i64)),
        (field("rank"), Value::Integer(rank)),
        (field("enabled"), Value::Boolean(n.is_multiple_of(2))),
        (
            field("label"),
            Value::String(format!(
                "item-{n:06}-version-{version:08}{}",
                "x".repeat(80)
            )),
        ),
        (field("payload"), Value::Binary(vec![version as u8; 1024])),
    ])
    .unwrap()
}
fn definition(which: usize) -> IndexDefinition {
    let ordered = |p, kind, order| IndexField::new(field(p), kind, order);
    let (id, fields, covers) = match which {
        0 => (
            "primary",
            vec![
                ordered("bucket", ScalarType::Integer, IndexOrder::Ascending),
                ordered("rank", ScalarType::Integer, IndexOrder::Descending),
            ],
            vec![field("label")],
        ),
        1 => (
            "rank",
            vec![ordered("rank", ScalarType::Integer, IndexOrder::Ascending)],
            vec![],
        ),
        2 => (
            "enabled-rank",
            vec![
                ordered("enabled", ScalarType::Boolean, IndexOrder::Descending),
                ordered("rank", ScalarType::Integer, IndexOrder::Ascending),
            ],
            vec![],
        ),
        _ => unreachable!(),
    };
    IndexDefinition::new(
        IndexId::new(id).unwrap(),
        IndexScope::Descendants(QueryScope::path("docs").unwrap()),
        fields,
        covers,
    )
    .unwrap()
}
fn query(projection: Projection) -> IndexedQuery {
    let definition = definition(0);
    IndexedQuery::new(
        definition.scope().clone(),
        vec![EqualityPredicate::new(field("bucket"), Value::Integer(0)).unwrap()],
        None,
        definition.fields()[1..].to_vec(),
        projection,
    )
    .unwrap()
    .with_index(definition.id().clone())
}
fn distribution(name: &str, size: usize, profile: &str, values: &[f64]) {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let at = |p: f64| values[((values.len() as f64 * p).ceil() as usize).saturating_sub(1)];
    println!(
        "latency {name}: size={size} profile={profile} samples={} p50={:.3} p95={:.3} p99={:.3} ms",
        values.len(),
        at(0.5),
        at(0.95),
        at(0.99)
    );
}
struct Bench {
    db: Database,
    metrics: Arc<InstrumentedStorage>,
    diag: Arc<InstrumentedMetadata>,
    serial: u64,
    ranks: BTreeMap<usize, i64>,
}
impl Bench {
    fn next(&mut self) -> u64 {
        self.serial += 1;
        self.serial
    }
    fn rank(&self, n: usize) -> i64 {
        self.ranks.get(&n).copied().unwrap_or(n as i64)
    }
    fn counters(&self) -> (Stats, [ObjectStats; 13]) {
        (self.metrics.stats(), KINDS.map(|k| self.diag.stats(k)))
    }
    fn report(
        &self,
        name: &str,
        size: usize,
        profile: &str,
        before: (Stats, [ObjectStats; 13]),
        elapsed: f64,
    ) {
        let (after, kinds) = self.counters();
        let (old, previous) = before;
        let kind = |n: usize| kinds[n].reads - previous[n].reads;
        let bytes = |n: usize| kinds[n].bytes_read - previous[n].bytes_read;
        let created = |n: usize| kinds[n].creates - previous[n].creates;
        println!(
            "| {name} | {size} | {profile} | {elapsed:.3} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            after.object_reads - old.object_reads,
            after.bytes_read - old.bytes_read,
            bytes(0),
            kind(1),
            kind(4),
            kind(5),
            kind(7),
            kind(11),
            created(5),
            after.object_creates - old.object_creates,
            after.bytes_written - old.bytes_written,
            after.head_conflicts - old.head_conflicts
        );
    }
    async fn measured<T>(
        &self,
        name: &str,
        size: usize,
        profile: &str,
        work: impl Future<Output = obelisk::Result<T>>,
    ) -> obelisk::Result<T> {
        let before = self.counters();
        let started = Instant::now();
        let result = work.await?;
        self.report(
            name,
            size,
            profile,
            before,
            started.elapsed().as_secs_f64() * 1000.0,
        );
        Ok(result)
    }
    async fn startup(&self, size: usize, profile: &str) -> obelisk::Result<()> {
        let before = self.counters();
        self.measured(
            "open",
            size,
            profile,
            Database::open_with_storage(self.metrics.clone(), DatabaseOptions::default()),
        )
        .await?;
        let (after, kinds) = self.counters();
        assert_eq!(after.object_reads - before.0.object_reads, 1);
        assert_eq!(kinds[0].reads - before.1[0].reads, 1);
        for (current, previous) in kinds.iter().zip(&before.1).skip(1) {
            assert_eq!(
                current.reads, previous.reads,
                "opening read a non-root object"
            );
        }
        Ok(())
    }
    async fn build(&mut self, size: usize, which: usize) -> obelisk::Result<IndexHandle> {
        let serial = self.next();
        let definition = definition(which);
        let created = self
            .db
            .create_index(&format!("create-{serial}"), definition)
            .await?;
        self.finish_build(size, &created.handle, "initial").await?;
        Ok(created.handle)
    }
    async fn finish_build(
        &mut self,
        size: usize,
        handle: &IndexHandle,
        label: &str,
    ) -> obelisk::Result<()> {
        let before = self.counters();
        let started = Instant::now();
        let mut samples = vec![];
        let mut examined = self
            .db
            .get_index(handle.id())
            .await?
            .and_then(|s| s.candidate)
            .map_or(0, |c| c.progress.examined());
        let mut chunks = 0;
        loop {
            let serial = self.next();
            let step = Instant::now();
            let receipt = self
                .db
                .advance_index_build(
                    &format!("build-{serial}"),
                    handle,
                    IndexBuildOptions::new(32)?,
                )
                .await?;
            samples.push(step.elapsed().as_secs_f64() * 1000.0);
            let current = receipt.progress.examined();
            assert!(current >= examined && current - examined <= 32);
            examined = current;
            chunks += 1;
            if chunks % 256 == 0 {
                println!(
                    "progress: {label} size={size} index={} examined={examined} chunks={chunks}",
                    handle.id()
                );
            }
            if receipt.state == IndexState::Active {
                break;
            }
            assert_eq!(receipt.state, IndexState::Building);
        }
        assert_eq!(examined, size as u64);
        self.report(
            "build-total",
            size,
            label,
            before,
            started.elapsed().as_secs_f64() * 1000.0,
        );
        distribution("build-chunk", size, label, &samples);
        println!(
            "build complete: size={size} index={} chunks={chunks} examined={examined} max_chunk=32 source_byte_limit=16777216",
            handle.id()
        );
        Ok(())
    }
    async fn reads(&self, size: usize, profile: &str) -> obelisk::Result<()> {
        let mut expected: Vec<_> = (0..size).filter(|n| n % 1000 == 0).collect();
        expected.sort_by(|a, b| self.rank(*b).cmp(&self.rank(*a)).then(a.cmp(b)));
        expected.truncate(8);
        for (name, projection) in [
            ("query-key", Projection::fields(vec![field("rank")])?),
            (
                "query-covered",
                Projection::fields(vec![field("rank"), field("label")])?,
            ),
            ("query-full", Projection::Full),
        ] {
            let query = query(projection);
            let records = self.diag.stats(ObjectKind::Record).reads;
            let page = self
                .measured(
                    name,
                    size,
                    profile,
                    self.db.query(&query, PageOptions::new(8)?, None),
                )
                .await?;
            assert_eq!(
                page.entries
                    .iter()
                    .map(|e| e.path.clone())
                    .collect::<Vec<_>>(),
                expected.iter().map(|n| path(*n)).collect::<Vec<_>>()
            );
            for (entry, n) in page.entries.iter().zip(&expected) {
                assert_eq!(
                    entry.document.get(&field("rank")),
                    Some(&Value::Integer(self.rank(*n)))
                );
            }
            assert_eq!(
                self.diag.stats(ObjectKind::Record).reads - records,
                if name == "query-full" {
                    page.entries.len() as u64
                } else {
                    0
                }
            );
        }
        Ok(())
    }
    async fn updates(&mut self, size: usize, profile: &str) -> obelisk::Result<()> {
        let mut samples = vec![];
        for _ in 0..8 {
            let serial = self.next();
            let n = size / 2;
            let rank = 1_000_000_000 + serial as i64 * 1_000_000 + n as i64;
            let started = Instant::now();
            self.measured(
                "update",
                size,
                profile,
                self.db.put(
                    &format!("update-{serial}"),
                    &path(n).to_string(),
                    document(n, rank, serial),
                    Condition::Any,
                ),
            )
            .await?;
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
            self.ranks.insert(n, rank);
        }
        distribution("foreground-update", size, profile, &samples);
        Ok(())
    }
    async fn contention(&mut self, size: usize, profile: &str) -> AnyResult<()> {
        let serial = self.next();
        let before = self.counters();
        let started = Instant::now();
        let mut wrappers = vec![];
        let mut tasks = vec![];
        let barrier = Arc::new(tokio::sync::Barrier::new(4));
        for writer in 0..4 {
            let accounting = Arc::new(publications::Publications::new(self.metrics.clone()));
            wrappers.push(accounting.clone());
            let db = Database::open_with_storage(accounting, DatabaseOptions::default()).await?;
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                let mut samples = vec![];
                let mut rank = 0;
                for step in 0..3 {
                    let version = serial * 100 + writer * 3 + step;
                    rank = 2_000_000_000 + version as i64 * 1_000_000 + writer as i64;
                    let began = Instant::now();
                    db.put(
                        &format!("concurrent-{serial}-{writer}-{step}"),
                        &path(writer as usize).to_string(),
                        document(writer as usize, rank, version),
                        Condition::Any,
                    )
                    .await?;
                    samples.push(began.elapsed().as_secs_f64() * 1000.0);
                }
                Ok::<_, obelisk::Error>((writer as usize, rank, samples))
            }));
        }
        let mut samples = vec![];
        for task in tasks {
            let (writer, rank, latencies) = task.await??;
            self.ranks.insert(writer, rank);
            samples.extend(latencies);
        }
        let mut totals = publications::Accounting::default();
        for wrapper in wrappers {
            totals.add(wrapper.stats());
        }
        let after = self.metrics.stats();
        assert_eq!(totals.committed.attempts, 12);
        assert_eq!(
            totals.abandoned.attempts,
            after.head_conflicts - before.0.head_conflicts
        );
        assert_eq!(totals.pending.objects, 0);
        assert_eq!(totals.unclassified.attempts, 0);
        assert_eq!(
            totals.committed.objects + totals.abandoned.objects,
            after.object_creates - before.0.object_creates
        );
        let head_bytes = after.bytes_written
            - before.0.bytes_written
            - totals.committed.bytes
            - totals.abandoned.bytes;
        self.report(
            "contention",
            size,
            profile,
            before,
            started.elapsed().as_secs_f64() * 1000.0,
        );
        distribution("contended-update", size, profile, &samples);
        println!(
            "uploads size={size} profile={profile}: {totals:?}; successful_head_bytes={head_bytes}"
        );
        Ok(())
    }
}

#[tokio::main]
async fn main() -> AnyResult<()> {
    let max = std::env::var("OBELISK_INDEX_SCALING_MAX")
        .unwrap_or_else(|_| "100000".into())
        .parse::<usize>()?;
    if !(100..=100_000).contains(&max) {
        return Err("OBELISK_INDEX_SCALING_MAX must be 100..=100000".into());
    }
    let folder = tempfile::tempdir()?;
    let is_folder = std::env::var("OBELISK_BASELINE_BACKEND").as_deref() != Ok("s3");
    let store: Arc<dyn Storage> = match std::env::var("OBELISK_BASELINE_BACKEND").as_deref() {
        Ok("s3") => {
            #[cfg(feature = "s3")]
            {
                let bucket = std::env::var("OBELISK_S3_BUCKET")?;
                let base = std::env::var("OBELISK_S3_TEST_PREFIX")?;
                let prefix = format!("{base}/index-scaling-{}", uuid::Uuid::new_v4());
                println!("S3 index-scaling prefix retained: {prefix}");
                Arc::new(storage::s3::S3::from_env(&bucket, &prefix)?)
            }
            #[cfg(not(feature = "s3"))]
            {
                return Err("enable s3".into());
            }
        }
        Ok("folder") | Err(_) => {
            #[cfg(feature = "folder")]
            {
                Arc::new(storage::folder::Folder::open(folder.path()).await?)
            }
            #[cfg(not(feature = "folder"))]
            {
                return Err("enable folder or choose s3".into());
            }
        }
        _ => return Err("unknown OBELISK_BASELINE_BACKEND".into()),
    };
    let diag = Arc::new(InstrumentedMetadata::new(store));
    let metrics = Arc::new(InstrumentedStorage::new(diag.clone()));
    let db = Database::open_with_storage(metrics.clone(), DatabaseOptions::default()).await?;
    let mut bench = Bench {
        db,
        diag,
        metrics,
        serial: 0,
        ranks: BTreeMap::new(),
    };
    let mut written = 0;
    println!(
        "Index scaling: OS={}/{} max={max}; payload=1024 binary bytes plus scalar fields and label; seed batches<=256; build chunks<=32 and 16MiB source; 8 serial updates and 4 writers x3 updates per profile. No engine cache; OS caches not flushed. Counters are logical adapter IO, not physical HTTP or allocator RSS. Profiles share one growing dataset and retained request history; indexes are dropped before the next size.",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "| Operation | Documents | Profile | ms | Object reads | Total read bytes incl HEAD | Root bytes | Document pages | Catalog pages | Scalar pages | Full records | Projections | Scalar pages created | Objects created | Written bytes incl HEAD | Conflicts |"
    );
    println!(
        "| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for size in [100, 1000, 10000, 100000] {
        if size > max {
            continue;
        }
        let before = bench.counters();
        let began = Instant::now();
        let mut samples = vec![];
        while written < size {
            let end = (written + 256).min(size);
            let serial = bench.next();
            let started = Instant::now();
            let mutations = (written..end)
                .map(|n| Mutation {
                    path: path(n),
                    condition: Condition::Any,
                    kind: MutationKind::Put(document(n, n as i64, 0)),
                })
                .collect();
            bench
                .db
                .commit(&Transaction::new(
                    RequestId::new(format!("fill-{serial}"))?,
                    mutations,
                    vec![],
                )?)
                .await?;
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
            written = end;
            if written.is_multiple_of(10240) {
                println!("fill progress: {written}/{size} documents");
            }
        }
        bench.report(
            "fill",
            size,
            "zero",
            before,
            began.elapsed().as_secs_f64() * 1000.0,
        );
        distribution("seed-batch", size, "zero", &samples);
        bench.startup(size, "zero").await?;
        assert!(matches!(
            bench
                .db
                .query(&query(Projection::Full), PageOptions::default(), None)
                .await,
            Err(Error::IndexUnavailable { .. })
        ));
        bench.updates(size, "zero").await?;
        bench.contention(size, "zero").await?;
        let primary = bench.build(size, 0).await?;
        bench.startup(size, "one").await?;
        bench.reads(size, "one").await?;
        bench.updates(size, "one").await?;
        bench.contention(size, "one").await?;
        bench.reads(size, "one-after-writes").await?;
        let second = bench.build(size, 1).await?;
        let third = bench.build(size, 2).await?;
        bench.startup(size, "three").await?;
        bench.reads(size, "three").await?;
        bench.updates(size, "three").await?;
        bench.contention(size, "three").await?;
        bench.reads(size, "three-after-writes").await?;
        let serial = bench.next();
        let replacement = bench
            .db
            .rebuild_index(&format!("rebuild-{serial}"), &primary)
            .await?;
        let serial = bench.next();
        let step = bench
            .db
            .advance_index_build(
                &format!("replacement-first-{serial}"),
                &replacement.handle,
                IndexBuildOptions::new(32)?,
            )
            .await?;
        assert_eq!(step.state, IndexState::Building);
        assert_eq!(step.progress.examined(), 32);
        bench.startup(size, "three+replacement").await?;
        bench.reads(size, "three+replacement").await?;
        bench.updates(size, "three+replacement").await?;
        bench.contention(size, "three+replacement").await?;
        bench.reads(size, "replacement-after-writes").await?;
        bench
            .finish_build(size, &replacement.handle, "replacement")
            .await?;
        bench.reads(size, "replacement-active").await?;
        for handle in [replacement.handle, second, third] {
            let serial = bench.next();
            bench
                .db
                .drop_index(&format!("drop-{serial}"), &handle)
                .await?;
        }
        assert!(bench.db.list_indexes().await?.is_empty());
        assert_eq!(bench.metrics.stats().list_calls, 0);
        assert_eq!(bench.metrics.stats().delete_calls, 0);
        if is_folder {
            let mut count = 0_u64;
            let mut bytes = 0_u64;
            for file in std::fs::read_dir(folder.path().join("objects"))? {
                count += 1;
                bytes += file?.metadata()?.len();
            }
            println!(
                "retained size={size}: objects={count} bytes={bytes}; includes history and abandoned preparations"
            );
        }
        println!(
            "checkpoint size={size}: logical totals {:?}",
            bench.metrics.stats()
        );
    }
    Ok(())
}
