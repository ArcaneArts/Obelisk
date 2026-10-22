//! Durable local POSIX folder storage. Network filesystems are unverified.
use crate::*;
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct Folder {
    root: PathBuf,
}
impl Folder {
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self> {
        if !cfg!(unix) {
            return Err(Error::Unsupported(
                "folder durability requires a local POSIX filesystem".into(),
            ));
        }
        let root = root.into();
        let this = Self { root };
        let root = this
            .run(|root| {
                durable_mkdir(&root)?;
                durable_mkdir(&root.join("objects"))?;
                let lock = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(root.join("LOCK"))?;
                lock.sync_all()?;
                sync_dir(&root)?;
                // Another opener may have created an ancestor without syncing it yet.
                let root = root.canonicalize()?;
                for directory in root.ancestors() {
                    sync_dir(directory)?;
                }
                Ok(root)
            })
            .await?;
        Ok(Self { root })
    }
    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(PathBuf) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || operation(root))
            .await
            .map_err(|e| Error::Transport(e.to_string()))?
    }
}
fn sync_dir(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}
fn durable_mkdir(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        durable_mkdir(parent)?;
    }
    match fs::create_dir(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => (),
        Err(e) => return Err(e),
    }
    sync_dir(path)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        sync_dir(parent)?;
    }
    Ok(())
}
fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::NotFound
        } else {
            e.into()
        }
    })?;
    if file.metadata()?.len() > limit as u64 {
        return Err(Error::Invalid("object exceeds byte limit".into()));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(Error::Invalid("object exceeds byte limit".into()));
    }
    Ok(bytes)
}
fn head(root: &Path) -> Result<Option<Head>> {
    let raw = match read_limited(&root.join("HEAD"), MAX_HEAD_BYTES + 37) {
        Ok(raw) => raw,
        Err(Error::NotFound) => return Ok(None),
        Err(e) => return Err(e),
    };
    if raw.len() < 37 || raw[36] != b'\n' {
        return Err(Error::Invalid("corrupt folder head token".into()));
    }
    let token = std::str::from_utf8(&raw[..36]).map_err(|_| Error::Invalid("head token".into()))?;
    ObjectId::parse(token)?;
    Ok(Some(Head {
        bytes: Bytes::copy_from_slice(&raw[37..]),
        version: VersionToken(token.into()),
    }))
}
fn lock(root: &Path, exclusive: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("LOCK"))?;
    if exclusive {
        FileExt::lock_exclusive(&file)?;
    } else {
        FileExt::lock_shared(&file)?;
    }
    Ok(file)
}
fn temporary(dir: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let path = dir.join(format!(".tmp-{}", ObjectId::random()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result?;
    Ok(path)
}
fn durable_head(
    root: &Path,
    flush: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<Option<Head>> {
    let _lock = lock(root, false)?;
    let observed = head(root)?;
    if observed.is_some() {
        // A publisher may have died after rename but before directory sync.
        // Resolve/retry can acknowledge this head, so first finish its barrier.
        // The shared lock prevents a different head replacing it while syncing.
        flush(root)?;
    }
    Ok(observed)
}
fn publish_head(
    root: &Path,
    expected: Option<&VersionToken>,
    bytes: &[u8],
    flush: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<VersionToken> {
    let _lock = lock(root, true)?;
    if head(root)?.map(|h| h.version).as_ref() != expected {
        return Err(Error::Conflict);
    }
    let token = ObjectId::random().to_string();
    let mut raw = token.as_bytes().to_vec();
    raw.push(b'\n');
    raw.extend_from_slice(bytes);
    let temp = temporary(root, &raw)?;
    if let Err(e) = fs::rename(&temp, root.join("HEAD")) {
        let _ = fs::remove_file(temp);
        return Err(e.into());
    }
    flush(root).map_err(|e| Error::Uncertain(e.to_string()))?;
    Ok(VersionToken(token))
}
#[async_trait]
impl Storage for Folder {
    async fn read(&self, id: ObjectId, range: Option<Range<u64>>) -> Result<Bytes> {
        self.run(move |root| {
            let path = root.join("objects").join(id.to_string());
            if let Some(range) = range {
                let mut file = File::open(path).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Error::NotFound
                    } else {
                        e.into()
                    }
                })?;
                let len = file.metadata()?.len();
                if len > MAX_OBJECT_BYTES as u64 {
                    return Err(Error::Invalid("object byte limit".into()));
                }
                validate_range(&range, len)?;
                file.seek(SeekFrom::Start(range.start))?;
                let mut bytes = vec![0; (range.end - range.start) as usize];
                file.read_exact(&mut bytes)?;
                Ok(bytes.into())
            } else {
                Ok(read_limited(&path, MAX_OBJECT_BYTES)?.into())
            }
        })
        .await
    }
    async fn create(&self, id: ObjectId, bytes: Bytes) -> Result<()> {
        if bytes.len() > MAX_OBJECT_BYTES {
            return Err(Error::Invalid("object byte limit".into()));
        }
        self.run(move |root| {
            let dir = root.join("objects");
            let temp = temporary(&dir, &bytes)?;
            let result = fs::hard_link(&temp, dir.join(id.to_string()));
            let _ = fs::remove_file(temp);
            match result {
                Ok(()) => {
                    sync_dir(&dir)?;
                    Ok(())
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(Error::Conflict),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }
    async fn read_head(&self) -> Result<Option<Head>> {
        self.run(|root| durable_head(&root, sync_dir)).await
    }
    async fn compare_exchange_head(
        &self,
        expected: Option<&VersionToken>,
        bytes: Bytes,
    ) -> Result<VersionToken> {
        if bytes.len() > MAX_HEAD_BYTES {
            return Err(Error::Invalid("head byte limit".into()));
        }
        let expected = expected.cloned();
        self.run(move |root| publish_head(&root, expected.as_ref(), &bytes, sync_dir))
            .await
    }
    async fn list(&self) -> Result<Vec<ObjectId>> {
        self.run(|root| {
            let mut ids = Vec::new();
            for entry in fs::read_dir(root.join("objects"))? {
                let entry = entry?;
                if let Some(name) = entry.file_name().to_str()
                    && let Ok(id) = ObjectId::parse(name)
                {
                    ids.push(id);
                }
            }
            ids.sort();
            Ok(ids)
        })
        .await
    }
    async fn delete(&self, id: ObjectId) -> Result<()> {
        self.run(move |root| {
            let dir = root.join("objects");
            match fs::remove_file(dir.join(id.to_string())) {
                Ok(()) => sync_dir(&dir)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
            Ok(())
        })
        .await
    }
}

#[cfg(all(test, unix))]
#[path = "folder_tests.rs"]
mod tests;
