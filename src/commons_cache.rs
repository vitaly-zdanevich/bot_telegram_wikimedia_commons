use crate::models::{CategoryHit, CategoryInfo, FileHit, Preferences, SearchQuery};
use anyhow::Result;
use bytes::Bytes;
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::hash::Hash;
use std::path::PathBuf;
use std::time::SystemTime;
use tokio::sync::{Mutex, RwLock};

/// Maximum file metadata objects kept in warm Lambda RAM per cache.
const FILE_METADATA_CACHE_MAX_ENTRIES: usize = 10_000;
/// Maximum search/category result objects kept in warm Lambda RAM per cache.
const RESULT_CACHE_MAX_ENTRIES: usize = 1_024;
/// Directory used for warm Lambda original-file byte caching.
const FILE_DISK_CACHE_DIR: &str = "/tmp/telegram-wikimedia-commons-bot-file-cache";

static FILE_BY_PAGE_ID_CACHE: Lazy<RwLock<HashMap<u64, FileHit>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static FILE_BY_TITLE_CACHE: Lazy<RwLock<HashMap<String, FileHit>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static FILE_SEARCH_CACHE: Lazy<RwLock<HashMap<String, Vec<FileHit>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static CATEGORY_SEARCH_CACHE: Lazy<RwLock<HashMap<String, Vec<CategoryHit>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static CATEGORY_INFO_CACHE: Lazy<RwLock<HashMap<String, CategoryInfo>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static CATEGORY_FILE_COUNT_CACHE: Lazy<RwLock<HashMap<String, u64>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static PAGE_TITLE_CACHE: Lazy<RwLock<HashMap<u64, Option<String>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static UPLOADER_NAME_CACHE: Lazy<RwLock<HashMap<String, Option<String>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static AUTH_COOKIE_HEADER_CACHE: Lazy<RwLock<HashMap<String, Option<String>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static FILE_BYTES_CACHE: Lazy<RwLock<FileBytesCache>> =
    Lazy::new(|| RwLock::new(FileBytesCache::new(file_bytes_cache_max_bytes())));
static FILE_DISK_CACHE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// A snapshot of warm Lambda Commons cache usage.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommonsCacheStats {
    /// File metadata entries cached by page id.
    pub file_by_page_id_entries: usize,
    /// File metadata entries cached by title.
    pub file_by_title_entries: usize,
    /// File-search result entries cached in RAM.
    pub file_search_entries: usize,
    /// Category-search result entries cached in RAM.
    pub category_search_entries: usize,
    /// Category info entries cached in RAM.
    pub category_info_entries: usize,
    /// Category file-count entries cached in RAM.
    pub category_file_count_entries: usize,
    /// Page-title lookup entries cached in RAM.
    pub page_title_entries: usize,
    /// Uploader canonical-name entries cached in RAM.
    pub uploader_name_entries: usize,
    /// SSM auth-cookie entries cached in RAM.
    pub auth_cookie_entries: usize,
    /// Original file byte objects cached in RAM.
    pub ram_file_bytes_entries: usize,
    /// Original file bytes cached in RAM.
    pub ram_file_bytes_bytes: usize,
    /// Maximum configured RAM byte-cache size.
    pub ram_file_bytes_max_bytes: usize,
    /// Original file byte objects cached under `/tmp`.
    pub tmp_file_bytes_entries: usize,
    /// Original file bytes cached under `/tmp`.
    pub tmp_file_bytes_bytes: u64,
    /// Maximum configured `/tmp` byte-cache size.
    pub tmp_file_bytes_max_bytes: u64,
}

impl CommonsCacheStats {
    /// Returns the total number of non-byte RAM cache entries.
    pub fn ram_metadata_entries(&self) -> usize {
        self.file_by_page_id_entries
            + self.file_by_title_entries
            + self.file_search_entries
            + self.category_search_entries
            + self.category_info_entries
            + self.category_file_count_entries
            + self.page_title_entries
            + self.uploader_name_entries
            + self.auth_cookie_entries
    }
}

/// Returns current warm Lambda Commons cache usage.
pub async fn cache_stats() -> CommonsCacheStats {
    let (ram_file_bytes_entries, ram_file_bytes_bytes, ram_file_bytes_max_bytes) = {
        let cache = FILE_BYTES_CACHE.read().await;
        (cache.entries(), cache.total_bytes(), cache.max_bytes())
    };
    let (tmp_file_bytes_entries, tmp_file_bytes_bytes) = match disk_cache_entries().await {
        Ok(entries) => (
            entries.len(),
            entries.iter().map(|entry| entry.size).sum::<u64>(),
        ),
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "failed to read /tmp Commons file cache stats"
            );
            (0, 0)
        }
    };

    CommonsCacheStats {
        file_by_page_id_entries: FILE_BY_PAGE_ID_CACHE.read().await.len(),
        file_by_title_entries: FILE_BY_TITLE_CACHE.read().await.len(),
        file_search_entries: FILE_SEARCH_CACHE.read().await.len(),
        category_search_entries: CATEGORY_SEARCH_CACHE.read().await.len(),
        category_info_entries: CATEGORY_INFO_CACHE.read().await.len(),
        category_file_count_entries: CATEGORY_FILE_COUNT_CACHE.read().await.len(),
        page_title_entries: PAGE_TITLE_CACHE.read().await.len(),
        uploader_name_entries: UPLOADER_NAME_CACHE.read().await.len(),
        auth_cookie_entries: AUTH_COOKIE_HEADER_CACHE.read().await.len(),
        ram_file_bytes_entries,
        ram_file_bytes_bytes,
        ram_file_bytes_max_bytes,
        tmp_file_bytes_entries,
        tmp_file_bytes_bytes,
        tmp_file_bytes_max_bytes: file_disk_cache_max_bytes(),
    }
}

/// Builds a warm-Lambda cache key for file search results.
pub(crate) fn file_search_cache_key(
    scope: &str,
    query: &SearchQuery,
    preferences: &Preferences,
    limit: usize,
    max_file_bytes: u64,
) -> String {
    format!("{scope}:{query:?}:{preferences:?}:{limit}:{max_file_bytes}")
}

/// Returns a cached auth cookie header.
pub(crate) async fn cached_auth_cookie_header(parameter: &str) -> Option<Option<String>> {
    AUTH_COOKIE_HEADER_CACHE
        .read()
        .await
        .get(parameter)
        .cloned()
}

/// Stores an auth cookie header in RAM.
pub(crate) async fn remember_auth_cookie_header(parameter: String, header: Option<String>) {
    let mut cache = AUTH_COOKIE_HEADER_CACHE.write().await;
    bounded_insert(&mut cache, parameter, header, RESULT_CACHE_MAX_ENTRIES);
}

/// Returns cached file search results when available.
pub(crate) async fn cached_file_search(key: &str) -> Option<Vec<FileHit>> {
    FILE_SEARCH_CACHE.read().await.get(key).cloned()
}

/// Stores file search results in warm Lambda RAM.
pub(crate) async fn put_file_search_cache(key: String, hits: Vec<FileHit>) {
    let mut cache = FILE_SEARCH_CACHE.write().await;
    bounded_insert(&mut cache, key, hits, RESULT_CACHE_MAX_ENTRIES);
}

/// Returns a cached canonical uploader name.
pub(crate) async fn cached_uploader_name(key: &str) -> Option<Option<String>> {
    UPLOADER_NAME_CACHE.read().await.get(key).cloned()
}

/// Stores a canonical uploader-name lookup result in RAM.
pub(crate) async fn remember_uploader_name(key: String, name: Option<String>) {
    let mut cache = UPLOADER_NAME_CACHE.write().await;
    bounded_insert(&mut cache, key, name, RESULT_CACHE_MAX_ENTRIES);
}

/// Returns cached file metadata by title, preserving caller order.
pub(crate) async fn cached_files_by_title(
    titles: &[String],
) -> (Vec<Option<FileHit>>, Vec<(usize, String)>) {
    let mut ordered_hits = vec![None; titles.len()];
    let mut missing = Vec::new();
    let cache = FILE_BY_TITLE_CACHE.read().await;
    for (index, title) in titles.iter().enumerate() {
        if let Some(hit) = cache.get(title).cloned() {
            ordered_hits[index] = Some(hit);
        } else {
            missing.push((index, title.clone()));
        }
    }
    (ordered_hits, missing)
}

/// Returns cached file metadata by page id, preserving caller order.
pub(crate) async fn cached_files_by_page_id(
    page_ids: &[u64],
) -> (Vec<Option<FileHit>>, Vec<(usize, u64)>) {
    let mut ordered_hits = vec![None; page_ids.len()];
    let mut missing = Vec::new();
    let cache = FILE_BY_PAGE_ID_CACHE.read().await;
    for (index, page_id) in page_ids.iter().enumerate() {
        if let Some(hit) = cache.get(page_id).cloned() {
            ordered_hits[index] = Some(hit);
        } else {
            missing.push((index, *page_id));
        }
    }
    (ordered_hits, missing)
}

/// Stores file metadata in page-id and title caches.
pub(crate) async fn remember_files(files: &[FileHit]) {
    if files.is_empty() {
        return;
    }
    {
        let mut cache = FILE_BY_PAGE_ID_CACHE.write().await;
        for file in files {
            if file.page_id != 0 {
                bounded_insert(
                    &mut cache,
                    file.page_id,
                    file.clone(),
                    FILE_METADATA_CACHE_MAX_ENTRIES,
                );
            }
        }
    }
    {
        let mut cache = FILE_BY_TITLE_CACHE.write().await;
        for file in files {
            if !file.title.trim().is_empty() {
                bounded_insert(
                    &mut cache,
                    file.title.clone(),
                    file.clone(),
                    FILE_METADATA_CACHE_MAX_ENTRIES,
                );
            }
        }
    }
}

/// Returns cached category search results.
pub(crate) async fn cached_category_search(key: &str) -> Option<Vec<CategoryHit>> {
    CATEGORY_SEARCH_CACHE.read().await.get(key).cloned()
}

/// Stores category search results in RAM.
pub(crate) async fn remember_category_search(key: String, categories: Vec<CategoryHit>) {
    let mut cache = CATEGORY_SEARCH_CACHE.write().await;
    bounded_insert(&mut cache, key, categories, RESULT_CACHE_MAX_ENTRIES);
}

/// Returns cached category info.
pub(crate) async fn cached_category_info(key: &str) -> Option<CategoryInfo> {
    CATEGORY_INFO_CACHE.read().await.get(key).cloned()
}

/// Stores category info in RAM.
pub(crate) async fn remember_category_info(key: String, info: CategoryInfo) {
    let mut cache = CATEGORY_INFO_CACHE.write().await;
    bounded_insert(&mut cache, key, info, RESULT_CACHE_MAX_ENTRIES);
}

/// Returns cached category file count.
pub(crate) async fn cached_category_file_count(title: &str) -> Option<u64> {
    CATEGORY_FILE_COUNT_CACHE.read().await.get(title).copied()
}

/// Stores category file count in RAM.
pub(crate) async fn remember_category_file_count(title: String, count: u64) {
    let mut cache = CATEGORY_FILE_COUNT_CACHE.write().await;
    bounded_insert(&mut cache, title, count, RESULT_CACHE_MAX_ENTRIES);
}

/// Returns cached page title.
pub(crate) async fn cached_page_title(page_id: u64) -> Option<Option<String>> {
    PAGE_TITLE_CACHE.read().await.get(&page_id).cloned()
}

/// Stores a page-title lookup in RAM.
pub(crate) async fn remember_page_title(page_id: u64, title: Option<String>) {
    let mut cache = PAGE_TITLE_CACHE.write().await;
    bounded_insert(&mut cache, page_id, title, RESULT_CACHE_MAX_ENTRIES);
}

/// Builds a cache key for original file bytes.
pub(crate) fn file_bytes_cache_key(file: &FileHit, url: &str) -> String {
    if let Some(sha1) = file.sha1.as_deref().filter(|value| !value.is_empty()) {
        format!("sha1:{sha1}")
    } else if file.page_id != 0 {
        format!("page:{}:{url}", file.page_id)
    } else {
        format!("url:{url}")
    }
}

/// Returns cached original file bytes from RAM or `/tmp`.
pub(crate) async fn cached_file_bytes(key: &str) -> Option<Bytes> {
    if let Some(bytes) = FILE_BYTES_CACHE.read().await.get(key) {
        return Some(bytes);
    }
    let bytes = read_disk_cached_file_bytes(key).await?;
    FILE_BYTES_CACHE
        .write()
        .await
        .insert(key.to_string(), bytes.clone());
    Some(bytes)
}

/// Stores original file bytes in RAM and in the `/tmp` fallback cache.
pub(crate) async fn remember_file_bytes(key: String, bytes: Bytes) {
    FILE_BYTES_CACHE
        .write()
        .await
        .insert(key.clone(), bytes.clone());
    if let Err(error) = write_disk_cached_file_bytes(&key, &bytes).await {
        tracing::warn!(
            error = %format!("{error:#}"),
            "failed to write Commons file bytes to /tmp cache"
        );
    }
}

/// Inserts into a map while bounding its entry count with best-effort eviction.
fn bounded_insert<K, V>(map: &mut HashMap<K, V>, key: K, value: V, max_entries: usize)
where
    K: Eq + Hash + Clone,
{
    if max_entries == 0 {
        return;
    }
    if !map.contains_key(&key)
        && map.len() >= max_entries
        && let Some(old_key) = map.keys().next().cloned()
    {
        map.remove(&old_key);
    }
    map.insert(key, value);
}

/// In-memory byte cache with a hard byte budget and best-effort eviction.
#[derive(Debug)]
struct FileBytesCache {
    max_bytes: usize,
    total_bytes: usize,
    files: HashMap<String, Bytes>,
}

impl FileBytesCache {
    /// Creates a byte cache with a hard maximum size.
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            total_bytes: 0,
            files: HashMap::new(),
        }
    }

    /// Returns cached bytes by key.
    fn get(&self, key: &str) -> Option<Bytes> {
        self.files.get(key).cloned()
    }

    /// Returns the number of cached byte objects.
    fn entries(&self) -> usize {
        self.files.len()
    }

    /// Returns cached byte usage.
    fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Returns the configured byte budget.
    fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Inserts bytes when the object can fit, evicting arbitrary old entries if needed.
    fn insert(&mut self, key: String, bytes: Bytes) -> bool {
        let size = bytes.len();
        if self.max_bytes == 0 || size > self.max_bytes {
            return false;
        }
        if let Some(existing) = self.files.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(existing.len());
        }
        while self.total_bytes.saturating_add(size) > self.max_bytes {
            let Some(old_key) = self.files.keys().next().cloned() else {
                return false;
            };
            if let Some(existing) = self.files.remove(&old_key) {
                self.total_bytes = self.total_bytes.saturating_sub(existing.len());
            }
        }
        self.total_bytes = self.total_bytes.saturating_add(size);
        self.files.insert(key, bytes);
        true
    }
}

/// Returns the RAM byte-cache budget.
fn file_bytes_cache_max_bytes() -> usize {
    let mb = env::var("FILE_BYTES_CACHE_MAX_MB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .or_else(|| {
            env::var("AWS_LAMBDA_FUNCTION_MEMORY_SIZE")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .map(|memory_mb| (memory_mb / 3).max(64))
        })
        .unwrap_or(512);
    mb.saturating_mul(1024 * 1024)
}

/// Returns the `/tmp` byte-cache budget.
fn file_disk_cache_max_bytes() -> u64 {
    let mb = env::var("FILE_DISK_CACHE_MAX_MB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(4_096);
    mb.saturating_mul(1024 * 1024)
}

/// Reads original file bytes from the `/tmp` fallback cache.
async fn read_disk_cached_file_bytes(key: &str) -> Option<Bytes> {
    let path = disk_cache_path(key);
    let bytes = tokio::fs::read(path).await.ok()?;
    Some(Bytes::from(bytes))
}

/// Writes original file bytes into the bounded `/tmp` fallback cache.
async fn write_disk_cached_file_bytes(key: &str, bytes: &Bytes) -> Result<()> {
    let max_bytes = file_disk_cache_max_bytes();
    if max_bytes == 0 || bytes.len() as u64 > max_bytes {
        return Ok(());
    }
    let _guard = FILE_DISK_CACHE_LOCK.lock().await;
    tokio::fs::create_dir_all(FILE_DISK_CACHE_DIR).await?;
    let path = disk_cache_path(key);
    if tokio::fs::metadata(&path).await.is_ok() {
        tokio::fs::remove_file(&path).await.ok();
    }
    evict_disk_cache_to_fit(bytes.len() as u64, max_bytes).await?;
    let tmp_path = path.with_extension(format!("tmp-{}", std::process::id()));
    tokio::fs::write(&tmp_path, bytes).await?;
    tokio::fs::rename(tmp_path, path).await?;
    Ok(())
}

/// Removes old `/tmp` cached files until the incoming object can fit.
async fn evict_disk_cache_to_fit(incoming_bytes: u64, max_bytes: u64) -> Result<()> {
    let mut entries = disk_cache_entries().await?;
    let mut total = entries.iter().map(|entry| entry.size).sum::<u64>();
    if total.saturating_add(incoming_bytes) <= max_bytes {
        return Ok(());
    }
    entries.sort_by_key(|entry| entry.modified);
    for entry in entries {
        if total.saturating_add(incoming_bytes) <= max_bytes {
            break;
        }
        if tokio::fs::remove_file(&entry.path).await.is_ok() {
            total = total.saturating_sub(entry.size);
        }
    }
    Ok(())
}

/// Lists files currently present in the `/tmp` byte cache.
async fn disk_cache_entries() -> Result<Vec<DiskCacheEntry>> {
    let mut entries = Vec::new();
    let mut dir = match tokio::fs::read_dir(FILE_DISK_CACHE_DIR).await {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = dir.next_entry().await? {
        let metadata = entry.metadata().await?;
        if !metadata.is_file() {
            continue;
        }
        entries.push(DiskCacheEntry {
            path: entry.path(),
            size: metadata.len(),
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    Ok(entries)
}

/// Metadata for one `/tmp` cache file.
struct DiskCacheEntry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

/// Builds a safe `/tmp` path for a cache key.
fn disk_cache_path(key: &str) -> PathBuf {
    let digest = Sha256::digest(key.as_bytes());
    PathBuf::from(FILE_DISK_CACHE_DIR).join(hex::encode(digest))
}

#[cfg(test)]
mod tests {
    use super::{FileBytesCache, bounded_insert};
    use bytes::Bytes;
    use std::collections::HashMap;

    #[test]
    fn bounded_insert_limits_cache_entries() {
        let mut cache = HashMap::new();
        bounded_insert(&mut cache, "a", 1, 2);
        bounded_insert(&mut cache, "b", 2, 2);
        bounded_insert(&mut cache, "c", 3, 2);

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("c"), Some(&3));
    }

    #[test]
    fn file_bytes_cache_skips_oversized_and_evicts_to_fit() {
        let mut cache = FileBytesCache::new(5);

        assert!(cache.insert("a".into(), Bytes::from_static(b"123")));
        assert!(cache.insert("b".into(), Bytes::from_static(b"45")));
        assert!(!cache.insert("huge".into(), Bytes::from_static(b"123456")));
        assert!(cache.insert("c".into(), Bytes::from_static(b"abcde")));

        assert_eq!(cache.total_bytes, 5);
        assert_eq!(cache.get("c").as_deref(), Some(&b"abcde"[..]));
        assert!(cache.get("huge").is_none());
    }
}
