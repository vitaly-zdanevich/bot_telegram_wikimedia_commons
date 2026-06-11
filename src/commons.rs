use crate::aws::AwsJsonClient;
use crate::config::Config;
use crate::models::{
    CategoryHit, CategoryInfo, Coordinates, DateFilter, FileHit, FileType, Preferences,
    SearchQuery, SizeOp,
};
use crate::parser::{
    is_audio_extension, is_image_extension, is_video_extension, normalize_category,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use once_cell::sync::Lazy;
use reqwest::{Client, header::COOKIE};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use time::{Date, Duration, OffsetDateTime};
use tokio::sync::{Mutex, OnceCell, RwLock};
use url::Url;

/// Default maximum Commons candidates fetched before local filtering.
const SEARCH_CANDIDATE_LIMIT: usize = 60;
/// Commons thumbnail width requested for Telegram image preview media groups.
const TELEGRAM_PREVIEW_THUMB_WIDTH: &str = "1280";
/// Commons GeoData radius used for inline location searches.
const NEARBY_SEARCH_RADIUS_METERS: u32 = 10_000;
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

/// HTTP client for Wikimedia Commons Action API.
#[derive(Clone)]
pub struct CommonsClient {
    client: Client,
    api_url: String,
    aws: AwsJsonClient,
    auth_cookie_parameter: Option<String>,
    auth_cookie_header: Arc<OnceCell<Option<String>>>,
}

impl CommonsClient {
    /// Creates a Commons API client from runtime configuration.
    pub fn new(config: &Config) -> Result<Self> {
        let client = Client::builder()
            .user_agent(config.user_agent.clone())
            .build()
            .context("failed to build Commons HTTP client")?;
        Ok(Self {
            client,
            api_url: config.commons_api_url.clone(),
            aws: AwsJsonClient::new(config.aws_region.clone()),
            auth_cookie_parameter: config.commons_auth_cookie_ssm_parameter.clone(),
            auth_cookie_header: Arc::new(OnceCell::new()),
        })
    }

    /// Sends a Commons API GET request with optional authenticated cookies.
    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T> {
        let mut request = self.client.get(url);
        if let Some(cookie_header) = self.auth_cookie_header().await {
            request = request.header(COOKIE, cookie_header);
        }
        Ok(request
            .send()
            .await?
            .error_for_status()?
            .json::<T>()
            .await?)
    }

    /// Returns a cached Cookie header loaded from SSM Parameter Store.
    async fn auth_cookie_header(&self) -> Option<String> {
        self.auth_cookie_header
            .get_or_init(|| async {
                match self.load_auth_cookie_header().await {
                    Ok(header) => header,
                    Err(error) => {
                        tracing::warn!(
                            error = %format!("{error:#}"),
                            "failed to load Commons auth cookie jar"
                        );
                        None
                    }
                }
            })
            .await
            .clone()
    }

    /// Loads and converts a Pywikibot LWP cookie jar from SSM.
    async fn load_auth_cookie_header(&self) -> Result<Option<String>> {
        let Some(parameter) = &self.auth_cookie_parameter else {
            return Ok(None);
        };
        if let Some(cached) = AUTH_COOKIE_HEADER_CACHE
            .read()
            .await
            .get(parameter)
            .cloned()
        {
            return Ok(cached);
        }
        if !self.aws.has_credentials() {
            return Ok(None);
        }
        let response = self
            .aws
            .post_json_1_1(
                "ssm",
                "AmazonSSM.GetParameter",
                json!({
                    "Name": parameter,
                    "WithDecryption": true
                }),
            )
            .await?;
        let Some(cookie_jar) = response["Parameter"]["Value"].as_str() else {
            return Ok(None);
        };
        let header = lwp_cookie_header(cookie_jar);
        {
            let mut cache = AUTH_COOKIE_HEADER_CACHE.write().await;
            bounded_insert(
                &mut cache,
                parameter.clone(),
                header.clone(),
                RESULT_CACHE_MAX_ENTRIES,
            );
        }
        Ok(header)
    }

    /// Searches Commons files according to the parsed query and preferences.
    pub async fn search_files(
        &self,
        query: &SearchQuery,
        preferences: &Preferences,
        limit: usize,
        max_file_bytes: u64,
    ) -> Result<Vec<FileHit>> {
        let cache_key = file_search_cache_key("files", query, preferences, limit, max_file_bytes);
        if let Some(cached) = cached_file_search(&cache_key).await {
            return Ok(cached);
        }
        if query.terms.is_empty() && query.user.is_some() && query.category.is_none() {
            let hits = self
                .search_files_by_uploader(query, preferences, limit, max_file_bytes)
                .await?;
            remember_files(&hits).await;
            put_file_search_cache(cache_key, hits.clone()).await;
            return Ok(hits);
        }

        let search = build_cirrus_query(query, preferences, Some(max_file_bytes));
        let mut url = Url::parse(&self.api_url)?;
        url.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("format", "json")
            .append_pair("formatversion", "2")
            .append_pair("generator", "search")
            .append_pair("gsrnamespace", "6")
            .append_pair("gsrlimit", &SEARCH_CANDIDATE_LIMIT.to_string())
            .append_pair("gsrsearch", &search)
            .append_pair("prop", "imageinfo")
            .append_pair("iilimit", "20")
            .append_pair("iiurlwidth", TELEGRAM_PREVIEW_THUMB_WIDTH)
            .append_pair("iiprop", imageinfo_props());

        let response = self.get_json::<QueryResponse>(url).await?;

        let mut hits = pages_to_hits(response.query.map(|query| query.pages).unwrap_or_default());
        apply_local_filters(&mut hits, query, preferences, max_file_bytes);
        if query.sort_by_size {
            hits.sort_by_key(|hit| hit.size_bytes);
        }
        hits.truncate(limit);
        remember_files(&hits).await;
        put_file_search_cache(cache_key, hits.clone()).await;
        Ok(hits)
    }

    /// Searches geotagged Commons files nearest to a latitude/longitude pair.
    pub async fn search_nearby_files(
        &self,
        latitude: f64,
        longitude: f64,
        query: &SearchQuery,
        preferences: &Preferences,
        limit: usize,
        max_file_bytes: u64,
    ) -> Result<Vec<FileHit>> {
        let cache_key = file_search_cache_key(
            &format!("nearby:{latitude:.5}:{longitude:.5}"),
            query,
            preferences,
            limit,
            max_file_bytes,
        );
        if let Some(cached) = cached_file_search(&cache_key).await {
            return Ok(cached);
        }
        let mut url = Url::parse(&self.api_url)?;
        url.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("format", "json")
            .append_pair("formatversion", "2")
            .append_pair("list", "geosearch")
            .append_pair("gsnamespace", "6")
            .append_pair("gscoord", &format!("{latitude}|{longitude}"))
            .append_pair("gsradius", &NEARBY_SEARCH_RADIUS_METERS.to_string())
            .append_pair("gslimit", &SEARCH_CANDIDATE_LIMIT.to_string());

        let response = self.get_json::<GeoSearchResponse>(url).await?;
        let page_ids = response
            .query
            .map(|query| {
                query
                    .geosearch
                    .into_iter()
                    .map(|hit| hit.pageid)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut hits = self.files_by_page_ids(&page_ids).await?;
        apply_local_filters(&mut hits, query, preferences, max_file_bytes);
        retain_title_terms(&mut hits, &query.terms);
        if query.sort_by_size {
            hits.sort_by_key(|hit| hit.size_bytes);
        }
        hits.truncate(limit);
        remember_files(&hits).await;
        put_file_search_cache(cache_key, hits.clone()).await;
        Ok(hits)
    }

    /// Lists recent files by uploader and applies the same local filters as search.
    async fn search_files_by_uploader(
        &self,
        query: &SearchQuery,
        preferences: &Preferences,
        limit: usize,
        max_file_bytes: u64,
    ) -> Result<Vec<FileHit>> {
        let user = normalize_username(query.user.as_deref().context("uploader is required")?);
        let hits = self
            .search_files_by_uploader_name(&user, query, preferences, limit, max_file_bytes)
            .await?;
        if !hits.is_empty() {
            return Ok(hits);
        }
        let Some(canonical_user) = self.resolve_uploader_name(&user).await? else {
            return Ok(hits);
        };
        if same_api_username(&canonical_user, &user) {
            return Ok(hits);
        }
        self.search_files_by_uploader_name(
            &canonical_user,
            query,
            preferences,
            limit,
            max_file_bytes,
        )
        .await
    }

    /// Lists recent files by one exact Commons uploader name.
    async fn search_files_by_uploader_name(
        &self,
        user: &str,
        query: &SearchQuery,
        preferences: &Preferences,
        limit: usize,
        max_file_bytes: u64,
    ) -> Result<Vec<FileHit>> {
        let mut hits = Vec::new();
        let mut aicontinue: Option<String> = None;
        let mut pages_seen = 0_u8;

        while hits.len() < limit && pages_seen < 8 {
            pages_seen += 1;
            let mut url = Url::parse(&self.api_url)?;
            url.query_pairs_mut()
                .append_pair("action", "query")
                .append_pair("format", "json")
                .append_pair("formatversion", "2")
                .append_pair("list", "allimages")
                .append_pair("aisort", "timestamp")
                .append_pair("aidir", "older")
                .append_pair("aiuser", user)
                .append_pair("ailimit", &SEARCH_CANDIDATE_LIMIT.to_string())
                .append_pair(
                    "aiprop",
                    "timestamp|user|url|size|dimensions|mime|mediatype|sha1",
                );
            if let Some(value) = &aicontinue {
                url.query_pairs_mut().append_pair("aicontinue", value);
            }

            let response = self.get_json::<AllImagesResponse>(url).await?;
            let titles = response
                .query
                .map(|query| query.allimages)
                .unwrap_or_default()
                .into_iter()
                .filter(|image| query.bypass_telegram_limit || image.size <= max_file_bytes)
                .map(|image| image.title)
                .collect::<Vec<_>>();
            if titles.is_empty() && response.continue_data.is_none() {
                break;
            }

            let mut page_hits = self.files_by_titles(&titles).await?;
            apply_local_filters(&mut page_hits, query, preferences, max_file_bytes);
            hits.extend(page_hits);
            hits.truncate(limit);
            aicontinue = response.continue_data.and_then(|data| data.aicontinue);
            if aicontinue.is_none() {
                break;
            }
        }
        if query.sort_by_size {
            hits.sort_by_key(|hit| hit.size_bytes);
        }
        hits.truncate(limit);
        Ok(hits)
    }

    /// Resolves a user-typed uploader name to the canonical Commons username.
    async fn resolve_uploader_name(&self, user: &str) -> Result<Option<String>> {
        let cache_key = normalize_username(user).to_lowercase();
        if let Some(cached) = UPLOADER_NAME_CACHE.read().await.get(&cache_key).cloned() {
            return Ok(cached);
        }
        let mut url = Url::parse(&self.api_url)?;
        url.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("format", "json")
            .append_pair("formatversion", "2")
            .append_pair("list", "prefixsearch")
            .append_pair("psnamespace", "2")
            .append_pair("pssearch", user)
            .append_pair("pslimit", "10");

        let response = self.get_json::<PrefixSearchResponse>(url).await?;
        let resolved = choose_canonical_username(
            user,
            response
                .query
                .prefixsearch
                .iter()
                .map(|hit| hit.title.as_str()),
        );
        {
            let mut cache = UPLOADER_NAME_CACHE.write().await;
            bounded_insert(
                &mut cache,
                cache_key,
                resolved.clone(),
                RESULT_CACHE_MAX_ENTRIES,
            );
        }
        Ok(resolved)
    }

    /// Loads full file metadata for file titles while preserving caller order.
    async fn files_by_titles(&self, titles: &[String]) -> Result<Vec<FileHit>> {
        if titles.is_empty() {
            return Ok(Vec::new());
        }
        let mut ordered_hits = vec![None; titles.len()];
        let mut missing = Vec::new();
        {
            let cache = FILE_BY_TITLE_CACHE.read().await;
            for (index, title) in titles.iter().enumerate() {
                if let Some(hit) = cache.get(title).cloned() {
                    ordered_hits[index] = Some(hit);
                } else {
                    missing.push((index, title.clone()));
                }
            }
        }
        for chunk in missing.chunks(10) {
            let chunk_titles = chunk
                .iter()
                .map(|(_, title)| title.clone())
                .collect::<Vec<_>>();
            let mut url = Url::parse(&self.api_url)?;
            url.query_pairs_mut()
                .append_pair("action", "query")
                .append_pair("format", "json")
                .append_pair("formatversion", "2")
                .append_pair("titles", &chunk_titles.join("|"))
                .append_pair("prop", "imageinfo")
                .append_pair("iilimit", "20")
                .append_pair("iiurlwidth", TELEGRAM_PREVIEW_THUMB_WIDTH)
                .append_pair("iiprop", imageinfo_props());
            let response = self.get_json::<QueryResponse>(url).await?;
            let page_hits =
                pages_to_hits(response.query.map(|query| query.pages).unwrap_or_default());
            remember_files(&page_hits).await;
            let by_title = page_hits
                .into_iter()
                .map(|hit| (hit.title.clone(), hit))
                .collect::<HashMap<_, _>>();
            for (index, title) in chunk {
                if let Some(hit) = by_title.get(title).cloned() {
                    ordered_hits[*index] = Some(hit);
                }
            }
        }
        Ok(ordered_hits.into_iter().flatten().collect())
    }

    /// Loads full file metadata for page IDs while preserving caller order.
    async fn files_by_page_ids(&self, page_ids: &[u64]) -> Result<Vec<FileHit>> {
        if page_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut ordered_hits = vec![None; page_ids.len()];
        let mut missing = Vec::new();
        {
            let cache = FILE_BY_PAGE_ID_CACHE.read().await;
            for (index, page_id) in page_ids.iter().enumerate() {
                if let Some(hit) = cache.get(page_id).cloned() {
                    ordered_hits[index] = Some(hit);
                } else {
                    missing.push((index, *page_id));
                }
            }
        }
        for chunk in missing.chunks(50) {
            let pageids = chunk
                .iter()
                .map(|(_, page_id)| page_id.to_string())
                .collect::<Vec<_>>()
                .join("|");
            let mut url = Url::parse(&self.api_url)?;
            url.query_pairs_mut()
                .append_pair("action", "query")
                .append_pair("format", "json")
                .append_pair("formatversion", "2")
                .append_pair("pageids", &pageids)
                .append_pair("prop", "imageinfo")
                .append_pair("iilimit", "20")
                .append_pair("iiurlwidth", TELEGRAM_PREVIEW_THUMB_WIDTH)
                .append_pair("iiprop", imageinfo_props());
            let response = self.get_json::<QueryResponse>(url).await?;
            let page_hits =
                pages_to_hits(response.query.map(|query| query.pages).unwrap_or_default());
            remember_files(&page_hits).await;
            let by_page_id = page_hits
                .into_iter()
                .map(|hit| (hit.page_id, hit))
                .collect::<HashMap<_, _>>();
            for (index, page_id) in chunk {
                if let Some(hit) = by_page_id.get(page_id).cloned() {
                    ordered_hits[*index] = Some(hit);
                }
            }
        }
        Ok(ordered_hits.into_iter().flatten().collect())
    }

    /// Looks up a single file by MediaWiki page id.
    pub async fn file_by_page_id(&self, page_id: u64) -> Result<Option<FileHit>> {
        Ok(self.files_by_page_ids(&[page_id]).await?.into_iter().next())
    }

    /// Searches Commons categories by name.
    pub async fn search_categories(&self, query: &str, limit: usize) -> Result<Vec<CategoryHit>> {
        let cache_key = format!("{}:{limit}", query.trim().to_lowercase());
        if let Some(cached) = CATEGORY_SEARCH_CACHE.read().await.get(&cache_key).cloned() {
            return Ok(cached);
        }
        let mut url = Url::parse(&self.api_url)?;
        url.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("format", "json")
            .append_pair("formatversion", "2")
            .append_pair("list", "search")
            .append_pair("srnamespace", "14")
            .append_pair("srlimit", &limit.to_string())
            .append_pair("srsearch", query);

        let response = self.get_json::<CategorySearchResponse>(url).await?;

        let categories = response
            .query
            .search
            .into_iter()
            .map(|hit| CategoryHit {
                page_id: hit.pageid,
                display_title: hit.title.trim_start_matches("Category:").to_string(),
                title: hit.title,
                file_count: None,
            })
            .collect::<Vec<_>>();
        {
            let mut cache = CATEGORY_SEARCH_CACHE.write().await;
            bounded_insert(
                &mut cache,
                cache_key,
                categories.clone(),
                RESULT_CACHE_MAX_ENTRIES,
            );
        }
        Ok(categories)
    }

    /// Loads category summary, first files, and first subcategories.
    pub async fn category_info(
        &self,
        category: &str,
        file_limit: usize,
        subcategory_limit: usize,
        max_file_bytes: u64,
    ) -> Result<CategoryInfo> {
        let title = format!("Category:{}", normalize_category(category));
        let cache_key = format!("{title}:{file_limit}:{subcategory_limit}:{max_file_bytes}");
        if let Some(cached) = CATEGORY_INFO_CACHE.read().await.get(&cache_key).cloned() {
            return Ok(cached);
        }
        let summary = self.category_summary(&title).await?;
        let files = self
            .category_files(&title, file_limit, max_file_bytes)
            .await
            .unwrap_or_default();
        let subcategories = self
            .category_subcategories(&title, subcategory_limit)
            .await
            .unwrap_or_default();

        let info = CategoryInfo {
            files,
            subcategories,
            ..summary
        };
        remember_files(&info.files).await;
        {
            let mut cache = CATEGORY_INFO_CACHE.write().await;
            bounded_insert(
                &mut cache,
                cache_key,
                info.clone(),
                RESULT_CACHE_MAX_ENTRIES,
            );
        }
        Ok(info)
    }

    /// Loads category info by MediaWiki page id.
    pub async fn category_info_by_page_id(
        &self,
        page_id: u64,
        file_limit: usize,
        subcategory_limit: usize,
        max_file_bytes: u64,
    ) -> Result<CategoryInfo> {
        let title = self
            .page_title(page_id)
            .await?
            .context("category page id not found")?;
        self.category_info(
            title.trim_start_matches("Category:"),
            file_limit,
            subcategory_limit,
            max_file_bytes,
        )
        .await
    }

    /// Counts direct file members in a category when requested by preferences.
    pub async fn category_file_count(&self, category: &str) -> Result<u64> {
        let title = format!("Category:{}", normalize_category(category));
        if let Some(cached) = CATEGORY_FILE_COUNT_CACHE.read().await.get(&title).copied() {
            return Ok(cached);
        }
        let mut count = 0_u64;
        let mut cmcontinue: Option<String> = None;

        loop {
            let mut url = Url::parse(&self.api_url)?;
            url.query_pairs_mut()
                .append_pair("action", "query")
                .append_pair("format", "json")
                .append_pair("formatversion", "2")
                .append_pair("list", "categorymembers")
                .append_pair("cmtitle", &title)
                .append_pair("cmtype", "file")
                .append_pair("cmlimit", "500")
                .append_pair("cmprop", "ids");
            if let Some(value) = &cmcontinue {
                url.query_pairs_mut().append_pair("cmcontinue", value);
            }

            let response = self.get_json::<CategoryMembersResponse>(url).await?;
            count += response
                .query
                .as_ref()
                .map(|query| query.categorymembers.len() as u64)
                .unwrap_or_default();
            cmcontinue = response.continue_data.and_then(|data| data.cmcontinue);
            if cmcontinue.is_none() {
                break;
            }
        }
        {
            let mut cache = CATEGORY_FILE_COUNT_CACHE.write().await;
            bounded_insert(&mut cache, title, count, RESULT_CACHE_MAX_ENTRIES);
        }
        Ok(count)
    }

    /// Downloads a Commons file into memory. Used only when Telegram cannot fetch by URL.
    pub async fn download_file(&self, file: &FileHit) -> Result<Bytes> {
        let url = file
            .url
            .as_deref()
            .context("file has no original URL to download")?;
        let cache_key = file_bytes_cache_key(file, url);
        if let Some(cached) = cached_file_bytes(&cache_key).await {
            return Ok(cached);
        }
        let bytes = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        remember_file_bytes(cache_key, bytes.clone()).await;
        Ok(bytes)
    }

    /// Loads category summary and Wikidata page properties.
    async fn category_summary(&self, title: &str) -> Result<CategoryInfo> {
        let mut url = Url::parse(&self.api_url)?;
        url.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("format", "json")
            .append_pair("formatversion", "2")
            .append_pair("titles", title)
            .append_pair("redirects", "1")
            .append_pair("prop", "extracts|pageprops")
            .append_pair("exintro", "1")
            .append_pair("explaintext", "1");

        let response = self.get_json::<QueryResponse>(url).await?;

        let page = response
            .query
            .and_then(|query| query.pages.into_iter().next())
            .unwrap_or_default();
        Ok(CategoryInfo {
            page_id: page.pageid.unwrap_or_default(),
            title: page.title.unwrap_or_else(|| title.to_string()),
            description: page.extract.filter(|value| !value.trim().is_empty()),
            wikidata_item: page.pageprops.and_then(|props| {
                props
                    .get("wikibase_item")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
            ..CategoryInfo::default()
        })
    }

    /// Loads the title for a MediaWiki page id.
    async fn page_title(&self, page_id: u64) -> Result<Option<String>> {
        if let Some(cached) = PAGE_TITLE_CACHE.read().await.get(&page_id).cloned() {
            return Ok(cached);
        }
        let mut url = Url::parse(&self.api_url)?;
        url.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("format", "json")
            .append_pair("formatversion", "2")
            .append_pair("pageids", &page_id.to_string())
            .append_pair("prop", "info");

        let response = self.get_json::<QueryResponse>(url).await?;
        let title = response
            .query
            .and_then(|query| query.pages.into_iter().next())
            .and_then(|page| page.title);
        {
            let mut cache = PAGE_TITLE_CACHE.write().await;
            bounded_insert(&mut cache, page_id, title.clone(), RESULT_CACHE_MAX_ENTRIES);
        }
        Ok(title)
    }

    /// Loads direct file members in a category and enriches them with imageinfo.
    async fn category_files(
        &self,
        title: &str,
        limit: usize,
        max_file_bytes: u64,
    ) -> Result<Vec<FileHit>> {
        let mut url = Url::parse(&self.api_url)?;
        url.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("format", "json")
            .append_pair("formatversion", "2")
            .append_pair("generator", "categorymembers")
            .append_pair("gcmtitle", title)
            .append_pair("gcmtype", "file")
            .append_pair("gcmlimit", &(limit * 2).max(limit).to_string())
            .append_pair("prop", "imageinfo")
            .append_pair("iilimit", "20")
            .append_pair("iiurlwidth", TELEGRAM_PREVIEW_THUMB_WIDTH)
            .append_pair("iiprop", imageinfo_props());

        let response = self.get_json::<QueryResponse>(url).await?;
        let mut hits = pages_to_hits(response.query.map(|query| query.pages).unwrap_or_default());
        hits.retain(|hit| hit.size_bytes <= max_file_bytes);
        hits.truncate(limit);
        remember_files(&hits).await;
        Ok(hits)
    }

    /// Loads direct subcategory members.
    async fn category_subcategories(&self, title: &str, limit: usize) -> Result<Vec<CategoryHit>> {
        let mut url = Url::parse(&self.api_url)?;
        url.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("format", "json")
            .append_pair("formatversion", "2")
            .append_pair("list", "categorymembers")
            .append_pair("cmtitle", title)
            .append_pair("cmtype", "subcat")
            .append_pair("cmlimit", &limit.to_string())
            .append_pair("cmprop", "ids|title");

        let response = self.get_json::<CategoryMembersResponse>(url).await?;
        Ok(response
            .query
            .map(|query| query.categorymembers)
            .unwrap_or_default()
            .into_iter()
            .map(|member| CategoryHit {
                page_id: member.pageid,
                display_title: member.title.trim_start_matches("Category:").to_string(),
                title: member.title,
                file_count: None,
            })
            .collect())
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

/// Builds a warm-Lambda cache key for file search results.
fn file_search_cache_key(
    scope: &str,
    query: &SearchQuery,
    preferences: &Preferences,
    limit: usize,
    max_file_bytes: u64,
) -> String {
    format!("{scope}:{query:?}:{preferences:?}:{limit}:{max_file_bytes}")
}

/// Returns cached file search results when available.
async fn cached_file_search(key: &str) -> Option<Vec<FileHit>> {
    FILE_SEARCH_CACHE.read().await.get(key).cloned()
}

/// Stores file search results in warm Lambda RAM.
async fn put_file_search_cache(key: String, hits: Vec<FileHit>) {
    let mut cache = FILE_SEARCH_CACHE.write().await;
    bounded_insert(&mut cache, key, hits, RESULT_CACHE_MAX_ENTRIES);
}

/// Stores file metadata in page-id and title caches.
async fn remember_files(files: &[FileHit]) {
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

/// Builds a cache key for original file bytes.
fn file_bytes_cache_key(file: &FileHit, url: &str) -> String {
    if let Some(sha1) = file.sha1.as_deref().filter(|value| !value.is_empty()) {
        format!("sha1:{sha1}")
    } else if file.page_id != 0 {
        format!("page:{}:{url}", file.page_id)
    } else {
        format!("url:{url}")
    }
}

/// Returns cached original file bytes from RAM or `/tmp`.
async fn cached_file_bytes(key: &str) -> Option<Bytes> {
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
async fn remember_file_bytes(key: String, bytes: Bytes) {
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

/// Builds the Commons CirrusSearch query string.
pub fn build_cirrus_query(
    query: &SearchQuery,
    preferences: &Preferences,
    max_file_bytes: Option<u64>,
) -> String {
    let mut terms = Vec::new();
    let text = query.term_text();
    if !text.is_empty() {
        terms.push(text);
    } else {
        terms.push("*".to_string());
    }

    let file_type = query
        .file_type
        .clone()
        .unwrap_or_else(|| preferences.file_type.clone());
    if let Some(value) = file_type.cirrus_filetype() {
        terms.push(format!("filetype:{value}"));
    }

    let extension = query
        .extension
        .clone()
        .or_else(|| preferences.extension.clone());
    if let Some(ext) = extension {
        if let Some(mime) = extension_to_mime(&ext) {
            terms.push(format!("filemime:\"{mime}\""));
        } else {
            terms.push(format!("intitle:/{}/", regex::escape(&format!(".{ext}"))));
        }
    }

    if let Some(category) = &query.category {
        terms.push(format!("incategory:\"{}\"", normalize_category(category)));
    }

    for size in &query.size_filters {
        let kib = (size.bytes / 1024).max(1);
        match size.op {
            SizeOp::GreaterThan => terms.push(format!("filesize:>{kib}")),
            SizeOp::LessThan => terms.push(format!("filesize:<{kib}")),
        }
    }

    if let Some(max_bytes) = max_file_bytes {
        let kib = (max_bytes / 1024).max(1);
        terms.push(format!("filesize:<{kib}"));
    }

    terms.join(" ")
}

/// Returns the requested `iiprop` list for file metadata.
fn imageinfo_props() -> &'static str {
    "timestamp|user|url|size|sha1|mime|mediatype|metadata|commonmetadata|extmetadata"
}

/// Maps common extensions to MIME types used by Commons search.
fn extension_to_mime(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "avif" => Some("image/avif"),
        "tif" | "tiff" => Some("image/tiff"),
        "bmp" => Some("image/bmp"),
        "svg" => Some("image/svg+xml"),
        "xcf" => Some("image/x-xcf"),
        "mp3" => Some("audio/mpeg"),
        "oga" | "ogg" => Some("audio/ogg"),
        "flac" => Some("audio/flac"),
        "wav" => Some("audio/wav"),
        "opus" => Some("audio/opus"),
        "mid" | "midi" => Some("audio/midi"),
        "webm" => Some("video/webm"),
        "ogv" => Some("video/ogg"),
        "mpeg" | "mpg" => Some("video/mpeg"),
        "mp4" => Some("video/mp4"),
        "pdf" => Some("application/pdf"),
        "djvu" | "djv" => Some("image/vnd.djvu"),
        "stl" => Some("model/stl"),
        _ => None,
    }
}

/// Converts MediaWiki query pages into file hits.
fn pages_to_hits(pages: Vec<ApiPage>) -> Vec<FileHit> {
    pages
        .into_iter()
        .filter_map(|page| {
            let title = page.title?;
            let page_id = page.pageid.unwrap_or_default();
            let mut imageinfo = page.imageinfo.unwrap_or_default();
            let latest = imageinfo.first().cloned()?;
            let version_count = Some(imageinfo.len());
            imageinfo.clear();
            Some(imageinfo_to_hit(page_id, title, latest, version_count))
        })
        .collect()
}

/// Converts a single `imageinfo` object into the internal file model.
fn imageinfo_to_hit(
    page_id: u64,
    title: String,
    info: ApiImageInfo,
    version_count: Option<usize>,
) -> FileHit {
    let file_name = title.trim_start_matches("File:").to_string();
    let extmetadata = info.extmetadata.unwrap_or_default();
    let metadata = info.metadata.unwrap_or_default();
    let commonmetadata = info.commonmetadata.unwrap_or_default();
    let license_short_name = metadata_value(&extmetadata, "LicenseShortName");
    let license_url = metadata_value(&extmetadata, "LicenseUrl");
    let artist = metadata_value(&extmetadata, "Artist").map(strip_html);
    let date_text = metadata_value(&extmetadata, "DateTimeOriginal")
        .or_else(|| metadata_value(&extmetadata, "DateTime"))
        .map(strip_html);
    let coordinates = parse_coordinates(&metadata).or_else(|| parse_coordinates(&commonmetadata));
    let animated = parse_animation_flag(&metadata);
    let caption_text = metadata_value(&extmetadata, "ObjectName").map(strip_html);
    let description_text = metadata_value(&extmetadata, "ImageDescription").map(strip_html);
    let camera_model =
        metadata_string(&metadata, "Model").or_else(|| metadata_string(&commonmetadata, "Model"));
    let exposure_time = first_metadata_string(&metadata, &commonmetadata, "ExposureTime")
        .and_then(|value| format_exposure_time(&value));
    let f_number = first_metadata_string(&metadata, &commonmetadata, "FNumber")
        .and_then(|value| format_f_number(&value));
    let iso_speed = first_metadata_string(&metadata, &commonmetadata, "ISOSpeedRatings")
        .and_then(|value| format_iso_speed(&value));
    let focal_length = first_metadata_string(&metadata, &commonmetadata, "FocalLength")
        .and_then(|value| format_focal_length(&value));

    FileHit {
        page_id,
        title,
        file_name,
        size_bytes: info.size.unwrap_or_default(),
        width: info.width,
        height: info.height,
        mime: info.mime,
        media_type: info.mediatype,
        url: info.url,
        description_url: info.descriptionurl,
        thumb_url: info.thumburl,
        uploader: info.user,
        timestamp: info.timestamp,
        sha1: info.sha1,
        version_count,
        license_short_name,
        license_url,
        caption_text,
        description_text,
        artist,
        date_text,
        camera_model,
        exposure_time,
        f_number,
        iso_speed,
        focal_length,
        coordinates,
        animated,
        duration_seconds: info
            .duration
            .or_else(|| metadata_number(&metadata, "duration")),
    }
}

/// Applies filters that the Commons search backend cannot express reliably.
fn apply_local_filters(
    hits: &mut Vec<FileHit>,
    query: &SearchQuery,
    preferences: &Preferences,
    max_file_bytes: u64,
) {
    hits.retain(|hit| {
        if !query.bypass_telegram_limit && hit.size_bytes > max_file_bytes {
            return false;
        }
        if let Some(user) = &query.user
            && !hit.uploader.as_deref().is_some_and(|value| {
                normalize_username(value).eq_ignore_ascii_case(&normalize_username(user))
            })
        {
            return false;
        }
        if let Some(date) = &query.date
            && !hit
                .timestamp
                .as_deref()
                .is_some_and(|timestamp| timestamp_matches(timestamp, date))
        {
            return false;
        }
        if !preferences.blacklist_uploaders.is_empty()
            && hit.uploader.as_ref().is_some_and(|uploader| {
                preferences
                    .blacklist_uploaders
                    .iter()
                    .any(|blocked| blocked.eq_ignore_ascii_case(uploader))
            })
        {
            return false;
        }
        if let Some(ext) = query
            .extension
            .as_ref()
            .or(preferences.extension.as_ref())
            .map(|value| value.to_ascii_lowercase())
            && hit.extension().as_deref() != Some(ext.as_str())
        {
            return false;
        }
        if query.size_filters.iter().any(|size| match size.op {
            SizeOp::GreaterThan => hit.size_bytes <= size.bytes,
            SizeOp::LessThan => hit.size_bytes >= size.bytes,
        }) {
            return false;
        }

        let effective_type = query.file_type.as_ref().unwrap_or(&preferences.file_type);
        match effective_type {
            FileType::All => true,
            FileType::Images => {
                hit.extension().is_some_and(|ext| is_image_extension(&ext))
                    || hit
                        .mime
                        .as_deref()
                        .is_some_and(|mime| mime.starts_with("image/"))
            }
            FileType::Audio => {
                hit.extension().is_some_and(|ext| is_audio_extension(&ext)) || hit.is_audio()
            }
            FileType::Video => {
                hit.extension().is_some_and(|ext| is_video_extension(&ext))
                    || hit
                        .mime
                        .as_deref()
                        .is_some_and(|mime| mime.starts_with("video/"))
            }
        }
    });
}

/// Keeps only files whose title or filename contains every free-text search term.
fn retain_title_terms(hits: &mut Vec<FileHit>, terms: &[String]) {
    let terms = terms
        .iter()
        .map(|term| term.to_lowercase())
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return;
    }
    hits.retain(|hit| {
        let haystack = format!("{} {}", hit.title, hit.file_name).to_lowercase();
        terms.iter().all(|term| haystack.contains(term))
    });
}

/// Normalizes Commons usernames for API filters and local comparisons.
fn normalize_username(value: &str) -> String {
    value.trim().replace('_', " ")
}

/// Returns true when two usernames are identical for case-sensitive API filters.
fn same_api_username(left: &str, right: &str) -> bool {
    normalize_username(left) == normalize_username(right)
}

/// Picks a canonical username from User-namespace prefix search titles.
fn choose_canonical_username<'a>(
    requested: &str,
    titles: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let requested = normalize_username(requested).to_lowercase();
    let mut first_user = None;
    for title in titles {
        let Some(user) = title.strip_prefix("User:") else {
            continue;
        };
        if user.contains('/') {
            continue;
        }
        if normalize_username(user).to_lowercase() == requested {
            return Some(user.to_string());
        }
        first_user.get_or_insert_with(|| user.to_string());
    }
    first_user
}

/// Returns true when a Commons timestamp satisfies the parsed date filter.
fn timestamp_matches(timestamp: &str, filter: &DateFilter) -> bool {
    let parsed = OffsetDateTime::parse(timestamp, &time::format_description::well_known::Rfc3339);
    let Ok(parsed) = parsed else {
        return false;
    };
    match filter {
        DateFilter::Year(year) => parsed.year() == *year,
        DateFilter::Day(day) => Date::parse(
            day,
            &time::macros::format_description!("[year]-[month]-[day]"),
        )
        .is_ok_and(|date| parsed.date() == date),
        DateFilter::RelativeDays(days) => {
            parsed >= OffsetDateTime::now_utc() - Duration::days(i64::from(*days))
        }
        DateFilter::PreviousMonth => parsed >= OffsetDateTime::now_utc() - Duration::days(31),
        DateFilter::PreviousYear => parsed >= OffsetDateTime::now_utc() - Duration::days(366),
    }
}

/// Extracts an extmetadata string.
fn metadata_value(map: &HashMap<String, ExtMetadataValue>, key: &str) -> Option<String> {
    map.get(key).and_then(|value| match value.value.as_ref()? {
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    })
}

/// Extracts a string-like image metadata field.
fn metadata_string(metadata: &[MetadataEntry], key: &str) -> Option<String> {
    metadata
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(key))
        .and_then(|entry| match &entry.value {
            Value::String(text) => Some(strip_html(text.clone())),
            Value::Number(number) => Some(number.to_string()),
            Value::Bool(value) => Some(value.to_string()),
            _ => None,
        })
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Extracts the first string-like metadata value from primary or common metadata.
fn first_metadata_string(
    metadata: &[MetadataEntry],
    commonmetadata: &[MetadataEntry],
    key: &str,
) -> Option<String> {
    metadata_string(metadata, key).or_else(|| metadata_string(commonmetadata, key))
}

/// Formats EXIF exposure time for display.
fn format_exposure_time(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.to_ascii_lowercase().contains("sec") {
        return Some(value.to_string());
    }
    if let Some((numerator, denominator)) = parse_fraction(value) {
        let seconds = numerator / denominator;
        if (denominator - 1.0).abs() < f64::EPSILON {
            return Some(format!("{} sec", format_decimal(seconds, 3)));
        }
        return Some(format!("{value} sec ({})", format_decimal(seconds, 3)));
    }
    parse_decimal(value)
        .map(|seconds| format!("{} sec", format_decimal(seconds, 3)))
        .or_else(|| Some(format!("{value} sec")))
}

/// Formats EXIF F-number for display.
fn format_f_number(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.to_ascii_lowercase().starts_with("f/") {
        return Some(value.to_string());
    }
    parse_rational_or_decimal(value)
        .map(|number| format!("f/{}", format_decimal(number, 2)))
        .or_else(|| Some(format!("f/{value}")))
}

/// Formats EXIF ISO speed rating for display.
fn format_iso_speed(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    parse_rational_or_decimal(value)
        .and_then(|number| {
            if number.is_finite() && number >= 0.0 {
                Some(format_integer_grouped(number.round() as u64))
            } else {
                None
            }
        })
        .or_else(|| Some(value.to_string()))
}

/// Formats EXIF focal length for display.
fn format_focal_length(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.to_ascii_lowercase().contains("mm") {
        return Some(value.to_string());
    }
    parse_rational_or_decimal(value)
        .map(|millimeters| format!("{} mm", format_decimal(millimeters, 1)))
        .or_else(|| Some(format!("{value} mm")))
}

/// Extracts a numeric image metadata field.
fn metadata_number(metadata: &[MetadataEntry], key: &str) -> Option<f64> {
    metadata
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(key))
        .and_then(|entry| entry.value.as_f64())
}

/// Parses coordinate fields from MediaWiki metadata.
fn parse_coordinates(metadata: &[MetadataEntry]) -> Option<Coordinates> {
    let lat = metadata_number(metadata, "GPSLatitude")
        .or_else(|| metadata_number(metadata, "Latitude"))
        .or_else(|| metadata_number(metadata, "lat"))?;
    let lon = metadata_number(metadata, "GPSLongitude")
        .or_else(|| metadata_number(metadata, "Longitude"))
        .or_else(|| metadata_number(metadata, "lon"))?;
    Some(Coordinates { lat, lon })
}

/// Parses GIF/WebP animation fields from MediaWiki metadata.
fn parse_animation_flag(metadata: &[MetadataEntry]) -> bool {
    metadata.iter().any(|entry| {
        let name = entry.name.to_ascii_lowercase();
        if name == "animated" {
            return entry.value.as_bool().unwrap_or(false);
        }
        if name == "framecount" {
            return entry.value.as_u64().unwrap_or(1) > 1;
        }
        false
    })
}

/// Parses either an EXIF rational string or a decimal number.
fn parse_rational_or_decimal(value: &str) -> Option<f64> {
    if let Some((numerator, denominator)) = parse_fraction(value) {
        return Some(numerator / denominator);
    }
    parse_decimal(value)
}

/// Parses an EXIF rational string such as `1/500`.
fn parse_fraction(value: &str) -> Option<(f64, f64)> {
    let (numerator, denominator) = value.split_once('/')?;
    let numerator = parse_decimal(numerator)?;
    let denominator = parse_decimal(denominator)?;
    if denominator == 0.0 {
        return None;
    }
    Some((numerator, denominator))
}

/// Parses a decimal value after trimming common whitespace.
fn parse_decimal(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok()
}

/// Formats a decimal with a bounded number of fractional digits.
fn format_decimal(value: f64, max_fraction_digits: usize) -> String {
    let mut output = format!("{value:.max_fraction_digits$}");
    while output.contains('.') && output.ends_with('0') {
        output.pop();
    }
    if output.ends_with('.') {
        output.pop();
    }
    output
}

/// Formats a positive integer with comma group separators.
fn format_integer_grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(ch);
    }
    output
}

/// Strips small HTML fragments returned by Commons extmetadata.
fn strip_html(value: String) -> String {
    html_escape::decode_html_entities(&RegexTagStripper::strip(&value))
        .trim()
        .to_string()
}

/// Converts a Pywikibot LWP cookie jar into a compact HTTP Cookie header.
fn lwp_cookie_header(cookie_jar: &str) -> Option<String> {
    let cookies = cookie_jar
        .lines()
        .filter_map(lwp_cookie_pair)
        .collect::<Vec<_>>();
    if cookies.is_empty() {
        raw_cookie_header(cookie_jar)
    } else {
        Some(cookies.join("; "))
    }
}

/// Accepts an already compact Cookie header from SSM.
fn raw_cookie_header(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.contains('=') && !trimmed.contains('\n') {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Extracts one `name=value` pair from a `Set-Cookie3` LWP cookie line.
fn lwp_cookie_pair(line: &str) -> Option<String> {
    let line = line.trim();
    let payload = line.strip_prefix("Set-Cookie3: ")?;
    if !payload.to_ascii_lowercase().contains("domain=\"") {
        return None;
    }
    if !payload.to_ascii_lowercase().contains("wikimedia.org") {
        return None;
    }
    let (name, rest) = payload.split_once('=')?;
    let (value, _) = rest.split_once(';')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some(format!("{name}={}", unquote_lwp_value(value.trim())))
}

/// Removes LWP double quotes and simple backslash escaping.
fn unquote_lwp_value(value: &str) -> String {
    let unquoted = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    let mut output = String::with_capacity(unquoted.len());
    let mut escaped = false;
    for ch in unquoted.chars() {
        if escaped {
            output.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            output.push(ch);
        }
    }
    if escaped {
        output.push('\\');
    }
    output
}

struct RegexTagStripper;

impl RegexTagStripper {
    /// Removes HTML tags from the small metadata fragments returned by Commons.
    fn strip(value: &str) -> String {
        static TAG_RE: once_cell::sync::Lazy<regex::Regex> =
            once_cell::sync::Lazy::new(|| regex::Regex::new(r"<[^>]+>").expect("valid tag regex"));
        TAG_RE.replace_all(value, "").into_owned()
    }
}

#[derive(Debug, Default, Deserialize)]
struct QueryResponse {
    query: Option<QueryData>,
}

#[derive(Debug, Default, Deserialize)]
struct QueryData {
    #[serde(default)]
    pages: Vec<ApiPage>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ApiPage {
    pageid: Option<u64>,
    title: Option<String>,
    #[serde(default)]
    imageinfo: Option<Vec<ApiImageInfo>>,
    extract: Option<String>,
    #[serde(default)]
    pageprops: Option<HashMap<String, Value>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ApiImageInfo {
    timestamp: Option<String>,
    user: Option<String>,
    url: Option<String>,
    descriptionurl: Option<String>,
    thumburl: Option<String>,
    size: Option<u64>,
    width: Option<u64>,
    height: Option<u64>,
    sha1: Option<String>,
    mime: Option<String>,
    mediatype: Option<String>,
    duration: Option<f64>,
    #[serde(default)]
    metadata: Option<Vec<MetadataEntry>>,
    #[serde(default)]
    commonmetadata: Option<Vec<MetadataEntry>>,
    #[serde(default)]
    extmetadata: Option<HashMap<String, ExtMetadataValue>>,
}

#[derive(Clone, Debug, Deserialize)]
struct MetadataEntry {
    name: String,
    value: Value,
}

#[derive(Clone, Debug, Deserialize)]
struct ExtMetadataValue {
    value: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct CategorySearchResponse {
    query: CategorySearchData,
}

#[derive(Debug, Deserialize)]
struct CategorySearchData {
    search: Vec<CategorySearchHit>,
}

#[derive(Debug, Deserialize)]
struct CategorySearchHit {
    pageid: u64,
    title: String,
}

#[derive(Debug, Deserialize)]
struct CategoryMembersResponse {
    #[serde(default)]
    query: Option<CategoryMembersData>,
    #[serde(rename = "continue")]
    continue_data: Option<CategoryContinueData>,
}

#[derive(Debug, Deserialize)]
struct CategoryMembersData {
    #[serde(default)]
    categorymembers: Vec<CategoryMember>,
}

#[derive(Debug, Deserialize)]
struct CategoryMember {
    pageid: u64,
    title: String,
}

#[derive(Debug, Deserialize)]
struct CategoryContinueData {
    cmcontinue: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GeoSearchResponse {
    query: Option<GeoSearchQuery>,
}

#[derive(Debug, Default, Deserialize)]
struct GeoSearchQuery {
    #[serde(default)]
    geosearch: Vec<GeoSearchHit>,
}

#[derive(Debug, Deserialize)]
struct GeoSearchHit {
    pageid: u64,
}

#[derive(Debug, Default, Deserialize)]
struct PrefixSearchResponse {
    query: PrefixSearchQuery,
}

#[derive(Debug, Default, Deserialize)]
struct PrefixSearchQuery {
    #[serde(default)]
    prefixsearch: Vec<PrefixSearchHit>,
}

#[derive(Debug, Deserialize)]
struct PrefixSearchHit {
    title: String,
}

#[derive(Debug, Default, Deserialize)]
struct AllImagesResponse {
    query: Option<AllImagesQuery>,
    #[serde(rename = "continue")]
    continue_data: Option<AllImagesContinueData>,
}

#[derive(Debug, Default, Deserialize)]
struct AllImagesQuery {
    allimages: Vec<AllImage>,
}

#[derive(Debug, Deserialize)]
struct AllImage {
    title: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
struct AllImagesContinueData {
    aicontinue: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{FileType, Preferences, SizeFilter};

    #[test]
    fn builds_query_with_extension_category_and_size() {
        let query = SearchQuery {
            terms: vec!["Minsk".into()],
            file_type: Some(FileType::Images),
            extension: Some("jpg".into()),
            category: Some("Belarus".into()),
            size_filters: vec![SizeFilter {
                op: SizeOp::GreaterThan,
                bytes: 10 * 1024 * 1024,
            }],
            ..SearchQuery::default()
        };
        let built = build_cirrus_query(&query, &Preferences::default(), Some(50 * 1024 * 1024));
        assert!(built.contains("Minsk"));
        assert!(built.contains("filetype:bitmap"));
        assert!(built.contains("filemime:\"image/jpeg\""));
        assert!(built.contains("incategory:\"Belarus\""));
        assert!(built.contains("filesize:>10240"));
        assert!(built.contains("filesize:<51200"));
    }

    #[test]
    fn local_filters_match_usernames_with_underscores() {
        let mut hits = vec![FileHit {
            uploader: Some("Vitaly Zdanevich".into()),
            size_bytes: 1024,
            ..FileHit::default()
        }];
        let query = SearchQuery {
            user: Some("Vitaly_Zdanevich".into()),
            ..SearchQuery::default()
        };

        apply_local_filters(&mut hits, &query, &Preferences::default(), 50 * 1024 * 1024);

        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn canonical_username_selection_handles_case_and_subpages() {
        let titles = ["User:Vitaly Zdanevich/common.js", "User:Vitaly Zdanevich"];
        assert_eq!(
            choose_canonical_username("vitaly_zdanevich", titles),
            Some("Vitaly Zdanevich".into())
        );
        assert!(!same_api_username("Vitaly_Zdanevich", "vitaly zdanevich"));
    }

    #[test]
    fn canonical_username_selection_handles_non_ascii_exact_case() {
        let titles = ["User:Красный", "User:Красный Мак"];
        assert_eq!(
            choose_canonical_username("Красный", titles),
            Some("Красный".into())
        );
    }

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

    #[test]
    fn local_filters_apply_size_predicates() {
        let mut hits = vec![
            FileHit {
                size_bytes: 512,
                ..FileHit::default()
            },
            FileHit {
                size_bytes: 2 * 1024 * 1024,
                ..FileHit::default()
            },
        ];
        let query = SearchQuery {
            size_filters: vec![SizeFilter {
                op: SizeOp::GreaterThan,
                bytes: 1024 * 1024,
            }],
            ..SearchQuery::default()
        };

        apply_local_filters(&mut hits, &query, &Preferences::default(), 50 * 1024 * 1024);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].size_bytes, 2 * 1024 * 1024);
    }

    #[test]
    fn retain_title_terms_requires_every_term() {
        let mut hits = vec![
            FileHit {
                title: "File:Minsk city hall.jpg".into(),
                file_name: "Minsk city hall.jpg".into(),
                ..FileHit::default()
            },
            FileHit {
                title: "File:Minsk station.jpg".into(),
                file_name: "Minsk station.jpg".into(),
                ..FileHit::default()
            },
        ];

        retain_title_terms(&mut hits, &["minsk".into(), "hall".into()]);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_name, "Minsk city hall.jpg");
    }

    #[test]
    fn detects_gif_and_webp_animation_metadata() {
        let metadata = vec![MetadataEntry {
            name: "frameCount".into(),
            value: Value::from(44),
        }];
        assert!(parse_animation_flag(&metadata));

        let metadata = vec![MetadataEntry {
            name: "animated".into(),
            value: Value::from(true),
        }];
        assert!(parse_animation_flag(&metadata));
    }

    #[test]
    fn extracts_camera_model_metadata() {
        let metadata = vec![MetadataEntry {
            name: "Model".into(),
            value: Value::from("Canon EOS 6D"),
        }];

        assert_eq!(
            metadata_string(&metadata, "model"),
            Some("Canon EOS 6D".into())
        );
    }

    #[test]
    fn formats_common_exif_details() {
        assert_eq!(
            format_exposure_time("1/500").as_deref(),
            Some("1/500 sec (0.002)")
        );
        assert_eq!(format_f_number("71/10").as_deref(), Some("f/7.1"));
        assert_eq!(format_iso_speed("1250").as_deref(), Some("1,250"));
        assert_eq!(format_focal_length("50/1").as_deref(), Some("50 mm"));
    }

    #[test]
    fn strips_metadata_html() {
        assert_eq!(
            strip_html("<span>Jane&nbsp;Doe</span>".into()),
            "Jane\u{a0}Doe"
        );
    }

    #[test]
    fn converts_lwp_cookie_jar_to_header() {
        let jar = r#"
Set-Cookie3: centralauth_User="Vitaly"; path="/"; domain=".wikimedia.org"; path_spec; expires="2030-01-01 00:00:00Z"; version=0
Set-Cookie3: commonswikiSession=abc123; path="/"; domain="commons.wikimedia.org"; path_spec; discard; version=0
Set-Cookie3: other=ignored; path="/"; domain="example.org"; path_spec; version=0
"#;

        assert_eq!(
            lwp_cookie_header(jar).as_deref(),
            Some("centralauth_User=Vitaly; commonswikiSession=abc123")
        );
    }

    #[test]
    fn unquotes_lwp_cookie_values() {
        assert_eq!(unquote_lwp_value(r#""a\"b\\c""#), r#"a"b\c"#);
        assert_eq!(unquote_lwp_value("plain"), "plain");
    }
}
