//! Atomic, source-bound snapshots for offline and stale catalog use.
use super::{CatalogError as Error, MAX_CATALOG_BYTES, Registry, parse};
use llm_router::CancellationToken;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAGIC: &[u8] = b"RAPIDLM-CATALOG-1\n";
const MAX_HEADER: usize = 4096;
pub const MAX_SOURCES: usize = 64;
const MAX_DIRECTORY_ENTRIES: usize = 4096;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    source: String,
    fetched_at: u64,
    sha256: String,
    bytes: usize,
}
#[derive(Clone, Debug)]
pub struct Cache {
    root: PathBuf,
    source: String,
    key: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Freshness {
    Fresh,
    Stale,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cached {
    pub registry: Registry,
    pub freshness: Freshness,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSummary {
    pub provenance: super::Provenance,
    pub freshness: Freshness,
    pub providers: usize,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Origin {
    Cache,
    Refreshed,
    Fallback { error: Error },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolution {
    pub cached: Cached,
    pub origin: Origin,
}
pub struct ResolveOptions {
    pub now: u64,
    pub ttl: u64,
    pub offline: bool,
    pub force: bool,
    pub proxy: Option<llm_router::providers::dial::ProxyConfig>,
    pub gate: Option<std::sync::Arc<dyn llm_router::providers::dial::DialGate>>,
}
impl Cache {
    pub fn new(root: PathBuf, source: &str) -> Result<Self, Error> {
        crate::provider_accounts::endpoint(source).map_err(|_| Error::InvalidSource)?;
        let source = crate::mcp_oauth::Url::parse(source)
            .map_err(|_| Error::InvalidSource)?
            .to_string();
        let key = format!("{:x}", Sha256::digest(source.as_bytes()));
        Ok(Self { root, source, key })
    }
    fn path(&self) -> PathBuf {
        self.root.join(format!("{}.snapshot", self.key))
    }
    /// Remove only this source's saved snapshot; imported profiles are untouched.
    pub fn forget(&self, cancel: &CancellationToken) -> Result<bool, Error> {
        cancel.check().map_err(|_| Error::Cancelled)?;
        reject_symlink(&self.root)?;
        if !self.root.exists() {
            return Ok(false);
        }
        let lock = registry_lock(&self.root, cancel)?;
        let result = (|| {
            let path = self.path();
            reject_symlink(&path)?;
            cancel.check().map_err(|_| Error::Cancelled)?;
            match std::fs::remove_file(path) {
                Ok(()) => {
                    #[cfg(unix)]
                    File::open(&self.root)
                        .and_then(|dir| dir.sync_all())
                        .map_err(|_| Error::Storage)?;
                    Ok(true)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(_) => Err(Error::Storage),
            }
        })();
        let _ = lock.unlock();
        result
    }
    /// Enumerate validated saved sources without creating the registry or dialing.
    pub fn sources(
        root: &Path,
        now: u64,
        ttl: u64,
        cancel: &CancellationToken,
    ) -> Result<Vec<SourceSummary>, Error> {
        let mut result = Vec::new();
        for path in snapshot_paths(root)? {
            cancel.check().map_err(|_| Error::Cancelled)?;
            reject_symlink(&path)?;
            let mut file = File::open(&path).map_err(|_| Error::Storage)?;
            let mut magic = vec![0; MAGIC.len()];
            file.read_exact(&mut magic)
                .map_err(|_| Error::CorruptCache)?;
            if magic != MAGIC {
                return Err(Error::CorruptCache);
            }
            let mut size = [0; 4];
            file.read_exact(&mut size)
                .map_err(|_| Error::CorruptCache)?;
            let len = u32::from_be_bytes(size) as usize;
            if len == 0 || len > MAX_HEADER {
                return Err(Error::CorruptCache);
            }
            let mut bytes = vec![0; len];
            file.read_exact(&mut bytes)
                .map_err(|_| Error::CorruptCache)?;
            let header: Header = serde_json::from_slice(&bytes).map_err(|_| Error::CorruptCache)?;
            let cache =
                Self::new(root.to_path_buf(), &header.source).map_err(|_| Error::CorruptCache)?;
            if cache.path() != path {
                return Err(Error::CorruptCache);
            }
            let cached = cache.load(now, ttl, cancel)?.ok_or(Error::CorruptCache)?;
            result.push(SourceSummary {
                providers: cached.registry.providers.len(),
                provenance: cached.registry.provenance,
                freshness: cached.freshness,
            });
        }
        result.sort_by(|a, b| a.provenance.source.cmp(&b.provenance.source));
        Ok(result)
    }
    /// Prefer a fresh snapshot, or explicitly refresh it. Offline mode never
    /// dials. A failed refresh returns the existing snapshot with its age and
    /// typed failure, while cancellation remains an error.
    pub fn resolve(
        &self,
        options: ResolveOptions,
        cancel: &CancellationToken,
    ) -> Result<Resolution, Error> {
        let ResolveOptions {
            now,
            ttl,
            offline,
            force,
            proxy,
            gate,
        } = options;
        if ttl > 30 * 24 * 60 * 60 || offline && force {
            return Err(Error::InvalidPolicy);
        }
        let cached = match self.load(now, ttl, cancel) {
            Ok(cached) => cached,
            Err(Error::CorruptCache) if !offline => None,
            Err(error) => return Err(error),
        };
        if offline
            || !force
                && cached
                    .as_ref()
                    .is_some_and(|cached| cached.freshness == Freshness::Fresh)
        {
            return Ok(Resolution {
                cached: cached.ok_or(Error::NoCache)?,
                origin: Origin::Cache,
            });
        }
        let refreshed = super::fetch(&self.source, proxy, gate, cancel)
            .and_then(|body| self.save(&body, now, cancel))
            .and_then(|stored| {
                self.load(now, ttl, cancel)?
                    .map(|cached| (cached, stored))
                    .ok_or(Error::Storage)
            });
        match refreshed {
            Ok((cached, stored)) => Ok(Resolution {
                cached,
                origin: if stored {
                    Origin::Refreshed
                } else {
                    Origin::Cache
                },
            }),
            Err(Error::Cancelled) => Err(Error::Cancelled),
            Err(error) => cached
                .map(|cached| Resolution {
                    cached,
                    origin: Origin::Fallback { error },
                })
                .ok_or(error),
        }
    }
    /// Loading a missing cache has no filesystem side effects and never dials.
    pub fn load(
        &self,
        now: u64,
        ttl: u64,
        cancel: &CancellationToken,
    ) -> Result<Option<Cached>, Error> {
        cancel.check().map_err(|_| Error::Cancelled)?;
        let path = self.path();
        reject_symlink(&path)?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::Storage),
        };
        let mut bytes = Vec::new();
        file.take((MAX_CATALOG_BYTES + MAX_HEADER + MAGIC.len() + 5) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Storage)?;
        if bytes.len() > MAX_CATALOG_BYTES + MAX_HEADER + MAGIC.len() + 4
            || !bytes.starts_with(MAGIC)
        {
            return Err(Error::CorruptCache);
        }
        let offset = MAGIC.len();
        let size: [u8; 4] = bytes
            .get(offset..offset + 4)
            .ok_or(Error::CorruptCache)?
            .try_into()
            .map_err(|_| Error::CorruptCache)?;
        let header_len = u32::from_be_bytes(size) as usize;
        if header_len == 0 || header_len > MAX_HEADER {
            return Err(Error::CorruptCache);
        }
        let begin = offset + 4;
        let end = begin.checked_add(header_len).ok_or(Error::CorruptCache)?;
        let header: Header =
            serde_json::from_slice(bytes.get(begin..end).ok_or(Error::CorruptCache)?)
                .map_err(|_| Error::CorruptCache)?;
        let body = bytes.get(end..).ok_or(Error::CorruptCache)?;
        if header.source != self.source
            || header.bytes != body.len()
            || body.len() > MAX_CATALOG_BYTES
            || header.sha256 != format!("{:x}", Sha256::digest(body))
        {
            return Err(Error::CorruptCache);
        }
        let registry =
            parse(&header.source, body, header.fetched_at).map_err(|_| Error::CorruptCache)?;
        cancel.check().map_err(|_| Error::Cancelled)?;
        let freshness = if now
            .checked_sub(header.fetched_at)
            .is_some_and(|age| age < ttl)
        {
            Freshness::Fresh
        } else {
            Freshness::Stale
        };
        Ok(Some(Cached {
            registry,
            freshness,
        }))
    }
    /// Validate before replacing the snapshot. The account-independent lock
    /// serializes writers; an older fetch can never replace a newer snapshot.
    pub fn save(
        &self,
        body: &[u8],
        fetched_at: u64,
        cancel: &CancellationToken,
    ) -> Result<bool, Error> {
        cancel.check().map_err(|_| Error::Cancelled)?;
        let registry = parse(&self.source, body, fetched_at)?;
        if registry
            .providers
            .values()
            .all(|provider| provider.models.is_empty())
        {
            return Err(Error::InvalidDocument);
        }
        private_dir(&self.root)?;
        // A registry-wide lock makes the source bound atomic across different URLs.
        let lock = registry_lock(&self.root, cancel)?;
        let result = (|| {
            let paths = snapshot_paths(&self.root)?;
            if !paths.contains(&self.path()) && paths.len() >= MAX_SOURCES {
                return Err(Error::TooManySources);
            }
            match self.load(fetched_at, 0, cancel) {
                Ok(Some(current)) if current.registry.provenance.fetched_at >= fetched_at => {
                    return Ok(false);
                }
                Ok(_) | Err(Error::CorruptCache) => {}
                Err(error) => return Err(error),
            }
            let header = Header {
                source: self.source.clone(),
                fetched_at,
                sha256: registry.provenance.sha256,
                bytes: body.len(),
            };
            let header = serde_json::to_vec(&header).map_err(|_| Error::Storage)?;
            if header.len() > MAX_HEADER {
                return Err(Error::Storage);
            }
            let destination = self.path();
            reject_symlink(&destination)?;
            let suffix = crate::mcp_oauth::random_token(12).map_err(|_| Error::Storage)?;
            let temporary = self.root.join(format!(".{}.{}.tmp", self.key, suffix));
            let written = (|| {
                let mut file = open_private(&temporary, true)?;
                file.write_all(MAGIC)
                    .and_then(|_| file.write_all(&(header.len() as u32).to_be_bytes()))
                    .and_then(|_| file.write_all(&header))
                    .and_then(|_| file.write_all(body))
                    .and_then(|_| file.sync_all())
                    .map_err(|_| Error::Storage)?;
                cancel.check().map_err(|_| Error::Cancelled)?;
                std::fs::rename(&temporary, &destination).map_err(|_| Error::Storage)?;
                #[cfg(unix)]
                File::open(&self.root)
                    .and_then(|dir| dir.sync_all())
                    .map_err(|_| Error::Storage)?;
                Ok(true)
            })();
            if written.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            written
        })();
        let _ = lock.unlock();
        result
    }
}
fn registry_lock(root: &Path, cancel: &CancellationToken) -> Result<File, Error> {
    let lock_path = root.join("registry.lock");
    reject_symlink(&lock_path)?;
    let lock = open_private(&lock_path, false)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cancel.check().map_err(|_| Error::Cancelled)?;
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(std::fs::TryLockError::WouldBlock) => return Err(Error::Busy),
            Err(_) => return Err(Error::Storage),
        }
    }
}
fn snapshot_paths(root: &Path) -> Result<Vec<PathBuf>, Error> {
    reject_symlink(root)?;
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(Error::Storage),
    };
    let mut paths = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            return Err(Error::TooManySources);
        }
        let entry = entry.map_err(|_| Error::Storage)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(key) = name.strip_suffix(".snapshot") else {
            continue;
        };
        if key.len() == 64
            && key
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            paths.push(entry.path());
            if paths.len() > MAX_SOURCES {
                return Err(Error::TooManySources);
            }
        }
    }
    Ok(paths)
}
fn reject_symlink(path: &Path) -> Result<(), Error> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Storage),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(Error::Storage),
        _ => Ok(()),
    }
}
fn private_dir(path: &Path) -> Result<(), Error> {
    reject_symlink(path)?;
    std::fs::create_dir_all(path).map_err(|_| Error::Storage)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| Error::Storage)?;
    }
    Ok(())
}
fn open_private(path: &Path, exclusive: bool) -> Result<File, Error> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|_| Error::Storage)
}

#[cfg(test)]
mod tests {
    use super::*;
    const SOURCE: &str = "https://models.example/api.json";
    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "rapid-catalog-cache-{}",
            ::protocol::SessionId::new()
        ))
    }
    #[test]
    fn source_registry_is_bounded_across_concurrent_writers_and_enumerates_offline() {
        let root = root();
        let cancel = CancellationToken::new();
        assert!(Cache::sources(&root, 100, 60, &cancel).unwrap().is_empty());
        assert!(!root.exists());
        let body = br#"{"p":{"models":{"m":{"id":"m","limit":{"context":100}}}}}"#;
        for index in 0..MAX_SOURCES - 1 {
            let source = format!("https://source-{index}.example/catalog");
            Cache::new(root.clone(), &source)
                .unwrap()
                .save(body, 100, &cancel)
                .unwrap();
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads = (0..2)
            .map(|index| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let source = format!("https://race-{index}.example/catalog");
                    let cache = Cache::new(root, &source).unwrap();
                    barrier.wait();
                    cache.save(body, 101, &CancellationToken::new())
                })
            })
            .collect::<Vec<_>>();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Ok(true)))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(Error::TooManySources)))
                .count(),
            1
        );
        let sources = Cache::sources(&root, 110, 60, &cancel).unwrap();
        assert_eq!(sources.len(), MAX_SOURCES);
        assert!(
            sources
                .windows(2)
                .all(|pair| pair[0].provenance.source < pair[1].provenance.source)
        );
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn snapshots_are_source_bound_atomic_and_report_stale_offline_data() {
        let root = root();
        let cache = Cache::new(root.clone(), SOURCE).unwrap();
        let cancel = CancellationToken::new();
        assert!(cache.load(100, 60, &cancel).unwrap().is_none());
        assert!(!root.exists());
        let first = br#"{"p":{"models":{"a":{"id":"a","limit":{"context":100}}}}}"#;
        let second = br#"{"p":{"models":{"b":{"id":"b","limit":{"context":200}}}}}"#;
        assert!(cache.save(first, 100, &cancel).unwrap());
        assert_eq!(
            cache.save(b"{}", 101, &cancel).unwrap_err(),
            Error::InvalidDocument
        );
        let fresh = cache.load(159, 60, &cancel).unwrap().unwrap();
        assert_eq!(fresh.freshness, Freshness::Fresh);
        assert!(fresh.registry.providers["p"].models.contains_key("a"));
        assert_eq!(
            cache.load(160, 60, &cancel).unwrap().unwrap().freshness,
            Freshness::Stale
        );
        assert_eq!(
            cache.load(90, 60, &cancel).unwrap().unwrap().freshness,
            Freshness::Stale
        );
        assert!(!cache.save(second, 99, &cancel).unwrap());
        assert!(!cache.save(second, 100, &cancel).unwrap());
        assert!(cache.save(second, 101, &cancel).unwrap());
        assert!(
            cache
                .load(101, 60, &cancel)
                .unwrap()
                .unwrap()
                .registry
                .providers["p"]
                .models
                .contains_key("b")
        );
        assert!(
            Cache::new(root.clone(), "https://other.example/api.json")
                .unwrap()
                .load(101, 60, &cancel)
                .unwrap()
                .is_none()
        );
        let mut bytes = std::fs::read(cache.path()).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(cache.path(), bytes).unwrap();
        assert!(matches!(
            cache.load(101, 60, &cancel),
            Err(Error::CorruptCache)
        ));
        assert!(cache.save(first, 102, &cancel).unwrap());
        assert!(
            cache
                .load(102, 60, &cancel)
                .unwrap()
                .unwrap()
                .registry
                .providers["p"]
                .models
                .contains_key("a")
        );
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn offline_and_failed_refresh_keep_the_existing_snapshot_and_explain_why() {
        use llm_router::providers::dial::{DialGate, DialTarget, ProxyTarget};
        use std::{net::SocketAddr, sync::Arc};
        struct Refuse;
        impl DialGate for Refuse {
            fn permit(
                &self,
                _: DialTarget<'_>,
                _: Option<&ProxyTarget>,
                _: &[SocketAddr],
            ) -> Result<Vec<SocketAddr>, llm_router::ProviderError> {
                Err(llm_router::ProviderError::Connection)
            }
        }
        let root = root();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let source = format!("http://{}/catalog", listener.local_addr().unwrap());
        let cache = Cache::new(root.clone(), &source).unwrap();
        let cancel = CancellationToken::new();
        let options = |offline, force| ResolveOptions {
            now: 1000,
            ttl: 60,
            offline,
            force,
            proxy: None,
            gate: Some(Arc::new(Refuse)),
        };
        assert_eq!(
            cache.resolve(options(true, false), &cancel).unwrap_err(),
            Error::NoCache
        );
        assert_eq!(
            cache.resolve(options(true, true), &cancel).unwrap_err(),
            Error::InvalidPolicy
        );
        let body = br#"{"p":{"models":{"a":{"id":"a","limit":{"context":100}}}}}"#;
        cache.save(body, 100, &cancel).unwrap();
        let offline = cache.resolve(options(true, false), &cancel).unwrap();
        assert_eq!(offline.origin, Origin::Cache);
        assert_eq!(offline.cached.freshness, Freshness::Stale);
        let fallback = cache.resolve(options(false, false), &cancel).unwrap();
        assert_eq!(
            fallback.origin,
            Origin::Fallback {
                error: Error::Network
            }
        );
        assert_eq!(fallback.cached.freshness, Freshness::Stale);
        assert!(listener.accept().is_err());
        cancel.cancel();
        assert_eq!(
            cache.resolve(options(false, false), &cancel).unwrap_err(),
            Error::Cancelled
        );
        let _ = std::fs::remove_dir_all(root);
    }
    #[cfg(unix)]
    #[test]
    fn cache_refuses_symlinked_snapshots_and_keeps_the_target_untouched() {
        use std::os::unix::fs::symlink;
        let root = root();
        std::fs::create_dir_all(&root).unwrap();
        let cache = Cache::new(root.clone(), SOURCE).unwrap();
        let target = root.join("target");
        std::fs::write(&target, b"untouched").unwrap();
        symlink(&target, cache.path()).unwrap();
        let cancel = CancellationToken::new();
        assert_eq!(cache.load(1, 1, &cancel).unwrap_err(), Error::Storage);
        assert_eq!(
            cache
                .save(
                    br#"{"p":{"models":{"a":{"id":"a","limit":{"context":10}}}}}"#,
                    1,
                    &cancel
                )
                .unwrap_err(),
            Error::Storage
        );
        assert_eq!(std::fs::read(target).unwrap(), b"untouched");
        let _ = std::fs::remove_dir_all(root);
    }
}
