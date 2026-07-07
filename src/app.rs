use crate::commons::CommonsClient;
use crate::config::Config;
use crate::idempotency::IdempotencyStore;
use crate::models::{
    COMMONS_AUDIO_EXTENSIONS, COMMONS_DOCUMENT_EXTENSIONS, COMMONS_IMAGE_EXTENSIONS,
    COMMONS_MODEL_EXTENSIONS, COMMONS_VIDEO_EXTENSIONS, DEFAULT_INLINE_RESULT_COUNT, DeliveryMode,
    DocumentPageMode, FileHit, FileType, INLINE_RESULT_COUNT_OPTIONS, Intent, Preferences,
    SearchQuery, normalize_inline_result_count,
};
use crate::pagination::{BUTTON_PAGE_SIZE, category_page, file_page};
use crate::parser::{is_commons_supported_extension, parse_intent, tokenize};
use crate::preferences::PreferenceStore;
use crate::stats::load_admin_stats;
use crate::telegram::{
    InlineKeyboardButton, InlineKeyboardMarkup, TelegramClient, Update, category_buttons_page,
    file_buttons_page, format_category_info, format_file_metadata, paginated_title,
    send_search_results,
};
use crate::wikidata::WikidataClient;
use anyhow::{Context, Result};
use lambda_http::{Body, Request, Response};
use serde_json::json;
use std::time::Duration;
use tokio::time::timeout;

const PAGINATED_RESULT_LIMIT: usize = BUTTON_PAGE_SIZE * 3;
const INLINE_QUERY_TIMEOUT: Duration = Duration::from_secs(6);
const INLINE_MAX_LOOKUP_RESULTS: usize = 151;
const UPDATE_IDEMPOTENCY_SECONDS: i64 = 24 * 60 * 60;
const CALLBACK_ACTION_IDEMPOTENCY_SECONDS: i64 = 30;

/// Handles one AWS Lambda HTTP request from Telegram.
pub async fn handle_lambda_request(request: Request) -> Result<Response<Body>> {
    let config = Config::from_env();
    verify_telegram_secret(&config, &request)?;
    if config.enable_test_endpoint && request.uri().path() == "/__test" {
        return handle_test_endpoint(&request);
    }
    let update: Update = serde_json::from_slice(request.body().as_ref())?;
    log_telegram_update(&update);
    let idempotency = IdempotencyStore::new(&config);
    let keys = idempotency_keys(&update);
    for key in &keys {
        match idempotency.reserve(&key.key, key.retention_seconds).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(
                    idempotency_key = %key.key,
                    "skipping duplicate Telegram update"
                );
                return ok_response();
            }
            Err(error) => tracing::warn!(
                idempotency_key = %key.key,
                error = %format!("{error:#}"),
                "failed to reserve Telegram update idempotency key"
            ),
        }
    }
    handle_update(update, &config).await?;
    for key in &keys {
        if let Err(error) = idempotency.mark_done(&key.key, key.retention_seconds).await {
            tracing::warn!(
                idempotency_key = %key.key,
                error = %format!("{error:#}"),
                "failed to mark Telegram update idempotency key as done"
            );
        }
    }
    ok_response()
}

/// Returns the standard Telegram webhook success response.
fn ok_response() -> Result<Response<Body>> {
    Ok(Response::builder()
        .status(200)
        .body(Body::Text("ok".into()))?)
}

/// One idempotency reservation key for a Telegram update.
#[derive(Clone, Debug, Eq, PartialEq)]
struct IdempotencyKey {
    /// DynamoDB/RAM key.
    key: String,
    /// How long duplicates should be suppressed.
    retention_seconds: i64,
}

/// Returns idempotency keys that suppress webhook retries and rapid file-button double clicks.
fn idempotency_keys(update: &Update) -> Vec<IdempotencyKey> {
    let mut keys = Vec::new();
    if let Some(update_id) = update.update_id {
        keys.push(IdempotencyKey {
            key: format!("TELEGRAM_UPDATE#{update_id}"),
            retention_seconds: UPDATE_IDEMPOTENCY_SECONDS,
        });
    }
    if let Some(callback) = &update.callback_query
        && let (Some(data), Some(message)) = (callback.data.as_deref(), callback.message.as_ref())
        && matches!(data.split_once(':'), Some(("file" | "cat", _)))
    {
        let message_id = message
            .message_id
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".into());
        keys.push(IdempotencyKey {
            key: format!(
                "TELEGRAM_CALLBACK_ACTION#{}#{}#{}#{}",
                callback.from.id, message.chat.id, message_id, data
            ),
            retention_seconds: CALLBACK_ACTION_IDEMPOTENCY_SECONDS,
        });
    }
    keys
}

/// Logs the Telegram identifiers needed to debug retries and duplicate replies.
fn log_telegram_update(update: &Update) {
    let kind = if update.callback_query.is_some() {
        "callback_query"
    } else if update.inline_query.is_some() {
        "inline_query"
    } else if update.message.is_some() {
        "message"
    } else {
        "unknown"
    };
    tracing::info!(
        update_id = ?update.update_id,
        update_kind = kind,
        callback_query_id = update
            .callback_query
            .as_ref()
            .map(|callback| callback.id.as_str())
            .unwrap_or_default(),
        callback_data = update
            .callback_query
            .as_ref()
            .and_then(|callback| callback.data.as_deref())
            .unwrap_or_default(),
        inline_query_id = update
            .inline_query
            .as_ref()
            .map(|query| query.id.as_str())
            .unwrap_or_default(),
        "received Telegram update"
    );
}

/// Handles the authenticated live-test endpoint without sending Telegram messages.
fn handle_test_endpoint(request: &Request) -> Result<Response<Body>> {
    let query = request
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(key, value)| {
            (key == "q").then(|| {
                urlencoding::decode(value)
                    .map(|decoded| decoded.into_owned())
                    .unwrap_or_else(|_| value.to_string())
            })
        })
        .unwrap_or_else(|| "jpg Minsk".into());
    let intent = parse_intent(&query);
    let payload = json!({
        "ok": true,
        "query": query,
        "intent": format!("{intent:?}"),
    });
    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Body::Text(payload.to_string()))?)
}

/// Handles one Telegram update.
pub async fn handle_update(update: Update, config: &Config) -> Result<()> {
    let token = config
        .telegram_bot_token
        .clone()
        .context("TELEGRAM_BOT_TOKEN is required")?;
    let telegram = TelegramClient::new(token);
    let commons = CommonsClient::new(config)?;
    let wikidata = WikidataClient::new(config.user_agent.clone())?;
    let preferences = PreferenceStore::new(config);

    if let Some(message) = update.message {
        let chat_id = message.chat.id;
        let user_id = message.from.as_ref().map(|user| user.id).unwrap_or(chat_id);
        telegram.send_typing(chat_id).await.ok();
        if let Some(location) = message.location {
            let prefs = preferences.get(user_id).await;
            let mut categories = commons
                .search_nearby_categories(location.latitude, location.longitude, BUTTON_PAGE_SIZE)
                .await?;
            if prefs.show_category_counts {
                enrich_category_counts(&commons, &mut categories).await;
            }
            telegram
                .send_closest_category_buttons(chat_id, &categories, prefs.pagination_enabled)
                .await?;
            return Ok(());
        }

        let text = message.text.unwrap_or_default();
        if is_preferences_update_command(&text) {
            let current = preferences.get(user_id).await;
            let updated = update_preferences(&text, current);
            preferences.put(user_id, &updated).await?;
            telegram
                .send_message(
                    chat_id,
                    &format_preferences(&updated, config),
                    Some(preferences_keyboard(&updated, PreferencesView::Main)),
                )
                .await?;
            return Ok(());
        }
        match parse_intent(&text) {
            Intent::Help => {
                let prefs = preferences.get(user_id).await;
                telegram
                    .send_message(chat_id, &help_text(config, &prefs), None)
                    .await?;
            }
            Intent::Preferences => {
                let prefs = preferences.get(user_id).await;
                telegram
                    .send_message(
                        chat_id,
                        &format_preferences(&prefs, config),
                        Some(preferences_keyboard(&prefs, PreferencesView::Main)),
                    )
                    .await?;
            }
            Intent::Stats => {
                if config.is_admin(user_id) {
                    let text = match load_admin_stats(config).await {
                        Ok(stats) => stats.render_text(config),
                        Err(error) => format!("Stats are not available yet: {error:#}"),
                    };
                    telegram.send_message(chat_id, &text, None).await?;
                } else {
                    telegram
                        .send_message(chat_id, "Only the admin can view stats.", None)
                        .await?;
                }
            }
            Intent::CategorySearch(query) => {
                let prefs = preferences.get(user_id).await;
                let mut categories = commons
                    .search_categories(&query, category_result_limit(&prefs))
                    .await?;
                if prefs.show_category_counts {
                    enrich_category_counts(&commons, &mut categories).await;
                }
                telegram
                    .send_category_buttons(chat_id, &categories, prefs.pagination_enabled)
                    .await?;
            }
            Intent::FileSearch(query) => {
                let prefs = preferences.get(user_id).await;
                let files = commons
                    .search_files(
                        &query,
                        &prefs,
                        file_result_limit(&query, &prefs),
                        config.max_file_bytes,
                    )
                    .await?;
                send_search_results(
                    &telegram,
                    chat_id,
                    &files,
                    &prefs,
                    query.links_flag,
                    query.image_previews_flag,
                )
                .await?;
                if !query.links_flag && !query.image_previews_flag {
                    let category_query = query.term_text();
                    if !category_query.is_empty() {
                        let mut categories = commons
                            .search_categories(&category_query, category_result_limit(&prefs))
                            .await?;
                        if prefs.show_category_counts {
                            enrich_category_counts(&commons, &mut categories).await;
                        }
                        telegram
                            .send_category_buttons(chat_id, &categories, prefs.pagination_enabled)
                            .await?;
                    }
                }
            }
            Intent::Empty => {
                telegram
                    .send_message(chat_id, &help_text(config, &Preferences::default()), None)
                    .await?;
            }
        }
    }

    if let Some(callback) = update.callback_query {
        let chat_id = callback
            .message
            .as_ref()
            .map(|message| message.chat.id)
            .context("callback query has no message")?;
        telegram.answer_callback_query(&callback.id).await.ok();
        telegram.send_typing(chat_id).await.ok();
        let prefs = preferences.get(callback.from.id).await;
        if let Some(data) = callback.data.as_deref() {
            if let Some(action) = data.strip_prefix("pref:") {
                if let Some(result) = apply_preference_callback(action, prefs) {
                    if result.changed {
                        preferences
                            .put(callback.from.id, &result.preferences)
                            .await?;
                    }
                    if result.render {
                        let message_id = callback
                            .message
                            .as_ref()
                            .and_then(|message| message.message_id);
                        send_or_edit_preferences(
                            &telegram,
                            chat_id,
                            message_id,
                            &result.preferences,
                            config,
                            result.view,
                        )
                        .await?;
                    }
                }
            } else if let Some((kind, token, page_index)) = parse_pagination_callback(data) {
                let message_id = callback
                    .message
                    .as_ref()
                    .and_then(|message| message.message_id)
                    .context("pagination callback has no editable message id")?;
                edit_paginated_results(
                    &telegram, chat_id, message_id, kind, token, page_index, &prefs,
                )
                .await?;
            } else if let Some(page_id) = data
                .strip_prefix("file:")
                .and_then(|value| value.parse::<u64>().ok())
            {
                if let Some(file) = commons.file_by_page_id(page_id).await? {
                    if file.size_bytes > config.max_file_bytes {
                        telegram
                            .send_message(
                                chat_id,
                                "This file is larger than Telegram's 50 MB bot limit.",
                                None,
                            )
                            .await?;
                    } else {
                        let caption = format_file_metadata(&file, &prefs);
                        telegram
                            .send_original_file(chat_id, &file, &commons, &caption)
                            .await?;
                    }
                }
            } else if let Some(page_id) = data
                .strip_prefix("cat:")
                .and_then(|value| value.parse::<u64>().ok())
            {
                let file_limit = category_file_result_limit(&prefs);
                let mut category = commons
                    .category_info_by_page_id(
                        page_id,
                        file_limit,
                        category_result_limit(&prefs),
                        config.max_file_bytes,
                    )
                    .await?;
                enrich_category_wikidata(&wikidata, &mut category).await;
                telegram
                    .send_message(chat_id, &format_category_info(&category), None)
                    .await?;
                send_category_files(&telegram, chat_id, &category.files, &prefs).await?;
                telegram
                    .send_subcategory_buttons(
                        chat_id,
                        &category.subcategories,
                        prefs.pagination_enabled,
                    )
                    .await?;
            }
        }
    }

    if let Some(inline_query) = update.inline_query {
        let query_id = inline_query.id;
        let query_text = inline_query.query;
        let result_offset = parse_inline_offset(&inline_query.offset);
        let user_id = inline_query.from.id;
        let location = inline_query.location;
        let inline_result = timeout(INLINE_QUERY_TIMEOUT, async {
            let prefs = preferences.get(user_id).await;
            let mut parsed = match parse_intent(&query_text) {
                Intent::FileSearch(query) => query,
                _ => Default::default(),
            };
            if parsed.file_type.is_none() {
                parsed.file_type = Some(FileType::Images);
            }
            let inline_result_count = prefs.normalized_inline_result_count();
            let use_location = location.is_some() && inline_location_applies(&parsed);
            let files = if let (Some(location), true) = (location, use_location) {
                commons
                    .search_nearby_files(
                        location.latitude,
                        location.longitude,
                        &parsed,
                        &prefs,
                        inline_lookup_limit(result_offset, inline_result_count),
                        config.max_file_bytes,
                    )
                    .await?
            } else {
                commons
                    .search_files(
                        &parsed,
                        &prefs,
                        inline_lookup_limit(result_offset, inline_result_count),
                        config.max_file_bytes,
                    )
                    .await?
            };
            Ok::<_, anyhow::Error>((files, use_location, inline_result_count))
        })
        .await;

        let (files, use_location, inline_result_count) = match inline_result {
            Ok(result) => result?,
            Err(_) => {
                tracing::warn!(
                    query = %query_text,
                    timeout_ms = INLINE_QUERY_TIMEOUT.as_millis(),
                    "inline search timed out before Telegram deadline"
                );
                (Vec::new(), false, DEFAULT_INLINE_RESULT_COUNT)
            }
        };
        let (files, next_offset) = inline_result_page(files, result_offset, inline_result_count);
        answer_inline_query_safely(
            &telegram,
            &query_id,
            &files,
            use_location,
            next_offset.as_deref(),
        )
        .await?;
    }

    Ok(())
}

/// Answers an inline query while ignoring Telegram's stale-query rejection.
async fn answer_inline_query_safely(
    telegram: &TelegramClient,
    query_id: &str,
    files: &[FileHit],
    is_personal: bool,
    next_offset: Option<&str>,
) -> Result<()> {
    match telegram
        .answer_inline_query(query_id, files, is_personal, next_offset)
        .await
    {
        Ok(()) => Ok(()),
        Err(error) if is_expired_inline_query_error(&error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "Telegram rejected an expired inline query"
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Detects Telegram's expected error for inline query IDs that are already stale.
fn is_expired_inline_query_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("query is too old")
        || message.contains("response timeout expired")
        || message.contains("query ID is invalid")
}

/// Parses Telegram inline pagination offsets, defaulting safely to the first page.
fn parse_inline_offset(offset: &str) -> usize {
    offset
        .parse::<usize>()
        .ok()
        .filter(|offset| *offset < INLINE_MAX_LOOKUP_RESULTS)
        .unwrap_or_default()
}

/// Returns how many Commons results are needed to render one inline page plus lookahead.
fn inline_lookup_limit(offset: usize, inline_result_count: usize) -> usize {
    let page_size = normalize_inline_result_count(inline_result_count);
    offset
        .saturating_add(page_size + 1)
        .min(INLINE_MAX_LOOKUP_RESULTS)
}

/// Slices one Telegram inline result page and returns the next offset when available.
fn inline_result_page(
    files: Vec<FileHit>,
    offset: usize,
    inline_result_count: usize,
) -> (Vec<FileHit>, Option<String>) {
    let page_size = normalize_inline_result_count(inline_result_count);
    let has_more = files.len() > offset.saturating_add(page_size);
    let page = files
        .into_iter()
        .skip(offset)
        .take(page_size)
        .collect::<Vec<_>>();
    let next_offset = has_more.then(|| (offset + page_size).to_string());
    (page, next_offset)
}

/// Returns true when inline location should rank results by nearby coordinates.
fn inline_location_applies(query: &SearchQuery) -> bool {
    query.user.is_none()
        && query.category.is_none()
        && query.date.is_none()
        && query.extension.is_none()
        && query.size_filters.is_empty()
        && matches!(query.file_type, None | Some(FileType::Images))
}

/// Adds rendered Wikidata claims to a category when a Wikidata item exists.
async fn enrich_category_wikidata(
    wikidata: &WikidataClient,
    category: &mut crate::models::CategoryInfo,
) {
    let Some(item) = category.wikidata_item.as_deref() else {
        return;
    };
    match wikidata.category_claims_html(item, "en").await {
        Ok(html) => category.wikidata_claims_html = Some(html),
        Err(error) => tracing::warn!(
            item,
            error = %format!("{error:#}"),
            "failed to load category Wikidata claims"
        ),
    }
}

/// Returns true when a message edits preferences through a slash command.
fn is_preferences_update_command(text: &str) -> bool {
    ["/settings ", "/prefs ", "/preferences "]
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

/// Sends or edits the preferences message with an inline settings keyboard.
async fn send_or_edit_preferences(
    telegram: &TelegramClient,
    chat_id: i64,
    message_id: Option<i64>,
    preferences: &Preferences,
    config: &Config,
    view: PreferencesView,
) -> Result<()> {
    let text = format_preferences(preferences, config);
    let keyboard = Some(preferences_keyboard(preferences, view));
    if let Some(message_id) = message_id {
        telegram
            .edit_message(chat_id, message_id, &text, keyboard)
            .await
    } else {
        telegram.send_message(chat_id, &text, keyboard).await
    }
}

/// Edits a paginated result message in place for a file or category callback.
async fn edit_paginated_results(
    telegram: &TelegramClient,
    chat_id: i64,
    message_id: i64,
    kind: &str,
    token: &str,
    page_index: usize,
    preferences: &Preferences,
) -> Result<()> {
    match kind {
        "f" => {
            let Some(page) = file_page(token, page_index).await else {
                return telegram
                    .edit_message(
                        chat_id,
                        message_id,
                        "Pagination expired. Run the search again.",
                        None,
                    )
                    .await;
            };
            telegram
                .edit_message(
                    chat_id,
                    message_id,
                    &paginated_title("Files", page.page_index, page.total_pages),
                    Some(InlineKeyboardMarkup {
                        inline_keyboard: file_buttons_page(
                            &page.items,
                            preferences,
                            Some(token),
                            page.page_index,
                            page.total_pages,
                        ),
                    }),
                )
                .await
        }
        "c" | "s" => {
            let Some(page) = category_page(token, page_index).await else {
                return telegram
                    .edit_message(
                        chat_id,
                        message_id,
                        "Pagination expired. Run the search again.",
                        None,
                    )
                    .await;
            };
            let title = if kind == "s" {
                "Subcategories"
            } else {
                "Categories"
            };
            telegram
                .edit_message(
                    chat_id,
                    message_id,
                    &paginated_title(title, page.page_index, page.total_pages),
                    Some(InlineKeyboardMarkup {
                        inline_keyboard: category_buttons_page(
                            &page.items,
                            Some(token),
                            page.page_index,
                            page.total_pages,
                            kind,
                        ),
                    }),
                )
                .await
        }
        _ => Ok(()),
    }
}

/// Parses a compact pagination callback payload.
fn parse_pagination_callback(data: &str) -> Option<(&str, &str, usize)> {
    let mut parts = data.split(':');
    if parts.next()? != "pg" {
        return None;
    }
    let kind = parts.next()?;
    let token = parts.next()?;
    let page_index = parts.next()?.parse::<usize>().ok()?;
    parts.next().is_none().then_some((kind, token, page_index))
}

/// Returns the file search limit needed by the selected Telegram delivery mode.
fn file_result_limit(query: &SearchQuery, preferences: &Preferences) -> usize {
    if preferences.pagination_enabled
        && preferences.delivery_mode == DeliveryMode::Buttons
        && !query.links_flag
        && !query.image_previews_flag
    {
        PAGINATED_RESULT_LIMIT
    } else {
        BUTTON_PAGE_SIZE
    }
}

/// Returns the category search limit needed by the category button renderer.
fn category_result_limit(preferences: &Preferences) -> usize {
    if preferences.pagination_enabled {
        PAGINATED_RESULT_LIMIT
    } else {
        BUTTON_PAGE_SIZE
    }
}

/// Returns the file limit needed after a category button click.
fn category_file_result_limit(preferences: &Preferences) -> usize {
    if preferences.category_file_buttons {
        category_result_limit(preferences)
    } else {
        BUTTON_PAGE_SIZE
    }
}

/// Sends direct category files according to the category-file preference.
async fn send_category_files(
    telegram: &TelegramClient,
    chat_id: i64,
    files: &[FileHit],
    preferences: &Preferences,
) -> Result<()> {
    if preferences.category_file_buttons {
        telegram
            .send_file_buttons(chat_id, files, preferences)
            .await
    } else {
        telegram
            .send_image_previews(chat_id, files, BUTTON_PAGE_SIZE, preferences, false)
            .await
    }
}

/// The currently visible preferences submenu.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreferencesView {
    /// Main preferences menu.
    Main,
    /// Extension group chooser.
    ExtensionMenu,
    /// Concrete extension buttons for one group.
    ExtensionGroup(ExtensionGroup),
}

/// Extension submenu groups.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExtensionGroup {
    /// Image extensions.
    Images,
    /// Audio extensions.
    Audio,
    /// Video extensions.
    Video,
    /// Document extensions.
    Documents,
    /// Model and other supported file extensions.
    Other,
}

impl ExtensionGroup {
    /// Parses callback data into an extension group.
    fn parse(value: &str) -> Option<Self> {
        match value {
            "images" => Some(Self::Images),
            "audio" => Some(Self::Audio),
            "video" => Some(Self::Video),
            "documents" => Some(Self::Documents),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    /// Returns callback data for this group.
    fn callback_value(self) -> &'static str {
        match self {
            Self::Images => "images",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Documents => "documents",
            Self::Other => "other",
        }
    }

    /// Returns a human-readable label for this group.
    fn label(self) -> &'static str {
        match self {
            Self::Images => "Images",
            Self::Audio => "Audio",
            Self::Video => "Video",
            Self::Documents => "Documents",
            Self::Other => "3D / other",
        }
    }

    /// Returns the extensions shown by this group.
    fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Images => COMMONS_IMAGE_EXTENSIONS,
            Self::Audio => COMMONS_AUDIO_EXTENSIONS,
            Self::Video => COMMONS_VIDEO_EXTENSIONS,
            Self::Documents => COMMONS_DOCUMENT_EXTENSIONS,
            Self::Other => COMMONS_MODEL_EXTENSIONS,
        }
    }
}

/// The result of applying one preference callback.
struct PreferenceCallbackResult {
    /// Updated preferences.
    preferences: Preferences,
    /// View to render next.
    view: PreferencesView,
    /// Whether the preference data changed.
    changed: bool,
    /// Whether the message should be re-rendered.
    render: bool,
}

/// Applies one inline preference button callback.
fn apply_preference_callback(
    action: &str,
    mut preferences: Preferences,
) -> Option<PreferenceCallbackResult> {
    let mut view = PreferencesView::Main;
    let mut changed = false;
    let mut render = true;

    match action {
        "main" => {}
        "ext" => view = PreferencesView::ExtensionMenu,
        "ext:off" => {
            view = PreferencesView::ExtensionMenu;
            changed = preferences.extension.take().is_some();
            render = changed;
        }
        "toggle:category-counts" => {
            preferences.show_category_counts = !preferences.show_category_counts;
            changed = true;
        }
        "toggle:sha1" => {
            preferences.show_sha1 = !preferences.show_sha1;
            changed = true;
        }
        "toggle:filesize" => {
            preferences.show_file_size = !preferences.show_file_size;
            changed = true;
        }
        "toggle:preview-metadata" => {
            preferences.show_preview_metadata = !preferences.show_preview_metadata;
            changed = true;
        }
        "toggle:rich-previews" => {
            preferences.rich_image_previews = !preferences.rich_image_previews;
            changed = true;
        }
        "toggle:category-file-buttons" => {
            preferences.category_file_buttons = !preferences.category_file_buttons;
            changed = true;
        }
        "toggle:pagination" => {
            preferences.pagination_enabled = !preferences.pagination_enabled;
            changed = true;
        }
        _ if action.starts_with("ext:group:") => {
            let group = action.trim_start_matches("ext:group:");
            view = PreferencesView::ExtensionGroup(ExtensionGroup::parse(group)?);
        }
        _ if action.starts_with("ext:set:") => {
            let ext = action.trim_start_matches("ext:set:");
            if !is_commons_supported_extension(ext) {
                return None;
            }
            view = PreferencesView::ExtensionGroup(extension_group_for(ext)?);
            let ext = ext.to_string();
            changed = preferences.extension.as_ref() != Some(&ext);
            preferences.extension = Some(ext);
            render = changed;
        }
        _ if action.starts_with("mode:") => {
            let mode = DeliveryMode::parse(action.trim_start_matches("mode:"))?;
            changed = preferences.delivery_mode != mode;
            preferences.delivery_mode = mode;
            render = changed;
        }
        _ if action.starts_with("type:") => {
            let file_type = FileType::parse(action.trim_start_matches("type:"))?;
            changed = preferences.file_type != file_type;
            preferences.file_type = file_type;
            render = changed;
        }
        _ if action.starts_with("inline:") => {
            let count = action
                .trim_start_matches("inline:")
                .parse::<usize>()
                .ok()
                .map(normalize_inline_result_count)?;
            changed = preferences.normalized_inline_result_count() != count;
            preferences.inline_result_count = count;
            render = changed;
        }
        _ if action.starts_with("pdf:") => {
            let mode = DocumentPageMode::parse(action.trim_start_matches("pdf:"))?;
            changed = preferences.pdf_mode != mode;
            preferences.pdf_mode = mode;
            render = changed;
        }
        _ if action.starts_with("djvu:") => {
            let mode = DocumentPageMode::parse(action.trim_start_matches("djvu:"))?;
            changed = preferences.djvu_mode != mode;
            preferences.djvu_mode = mode;
            render = changed;
        }
        _ => return None,
    }

    Some(PreferenceCallbackResult {
        preferences,
        view,
        changed,
        render,
    })
}

/// Returns the preference keyboard for one view.
fn preferences_keyboard(preferences: &Preferences, view: PreferencesView) -> InlineKeyboardMarkup {
    let rows = match view {
        PreferencesView::Main => main_preferences_keyboard(preferences),
        PreferencesView::ExtensionMenu => extension_menu_keyboard(preferences),
        PreferencesView::ExtensionGroup(group) => extension_group_keyboard(preferences, group),
    };
    InlineKeyboardMarkup {
        inline_keyboard: rows,
    }
}

/// Builds the main preferences keyboard.
fn main_preferences_keyboard(preferences: &Preferences) -> Vec<Vec<InlineKeyboardButton>> {
    vec![
        vec![
            pref_button(
                checked_label(
                    preferences.delivery_mode == DeliveryMode::Buttons,
                    "Buttons",
                ),
                "pref:mode:buttons",
            ),
            pref_button(
                checked_label(
                    preferences.delivery_mode == DeliveryMode::Links10,
                    "Links10",
                ),
                "pref:mode:links10",
            ),
        ],
        vec![
            pref_button(
                checked_label(
                    preferences.delivery_mode == DeliveryMode::Images10,
                    "Images10",
                ),
                "pref:mode:images10",
            ),
            pref_button(
                checked_label(
                    preferences.delivery_mode == DeliveryMode::Images20,
                    "Images20",
                ),
                "pref:mode:images20",
            ),
        ],
        vec![
            pref_button(
                checked_label(preferences.file_type == FileType::All, "All"),
                "pref:type:all",
            ),
            pref_button(
                checked_label(preferences.file_type == FileType::Images, "Images"),
                "pref:type:images",
            ),
        ],
        vec![
            pref_button(
                checked_label(preferences.file_type == FileType::Audio, "Audio"),
                "pref:type:audio",
            ),
            pref_button(
                checked_label(preferences.file_type == FileType::Video, "Video"),
                "pref:type:video",
            ),
        ],
        vec![
            pref_button(
                toggle_label(preferences.show_category_counts, "Category counts"),
                "pref:toggle:category-counts",
            ),
            pref_button(
                toggle_label(preferences.show_file_size, "File size labels"),
                "pref:toggle:filesize",
            ),
        ],
        vec![
            pref_button(
                toggle_label(preferences.show_preview_metadata, "Preview metadata"),
                "pref:toggle:preview-metadata",
            ),
            pref_button(
                toggle_label(preferences.rich_image_previews, "Rich previews"),
                "pref:toggle:rich-previews",
            ),
        ],
        vec![pref_button(
            toggle_label(preferences.category_file_buttons, "Category file buttons"),
            "pref:toggle:category-file-buttons",
        )],
        vec![
            pref_button(
                toggle_label(preferences.show_sha1, "SHA-1"),
                "pref:toggle:sha1",
            ),
            pref_button(
                toggle_label(preferences.pagination_enabled, "Pagination"),
                "pref:toggle:pagination",
            ),
        ],
        INLINE_RESULT_COUNT_OPTIONS
            .iter()
            .map(|count| {
                pref_button(
                    checked_label(
                        preferences.normalized_inline_result_count() == *count,
                        &format!("Inline {count}"),
                    ),
                    format!("pref:inline:{count}"),
                )
            })
            .collect(),
        vec![
            pref_button(
                checked_label(
                    preferences.pdf_mode == DocumentPageMode::Original,
                    "PDF original",
                ),
                "pref:pdf:original",
            ),
            pref_button(
                checked_label(
                    preferences.pdf_mode == DocumentPageMode::RenderedPages,
                    "PDF pages",
                ),
                "pref:pdf:rendered",
            ),
        ],
        vec![
            pref_button(
                checked_label(
                    preferences.djvu_mode == DocumentPageMode::Original,
                    "DjVu original",
                ),
                "pref:djvu:original",
            ),
            pref_button(
                checked_label(
                    preferences.djvu_mode == DocumentPageMode::RenderedPages,
                    "DjVu pages",
                ),
                "pref:djvu:rendered",
            ),
        ],
        vec![pref_button(
            format!(
                "Extension: {}",
                preferences.extension.as_deref().unwrap_or("all")
            ),
            "pref:ext",
        )],
    ]
}

/// Builds the extension group chooser keyboard.
fn extension_menu_keyboard(preferences: &Preferences) -> Vec<Vec<InlineKeyboardButton>> {
    vec![
        vec![pref_button(
            checked_label(preferences.extension.is_none(), "All extensions"),
            "pref:ext:off",
        )],
        vec![
            extension_group_button(preferences, ExtensionGroup::Images),
            extension_group_button(preferences, ExtensionGroup::Audio),
        ],
        vec![
            extension_group_button(preferences, ExtensionGroup::Video),
            extension_group_button(preferences, ExtensionGroup::Documents),
        ],
        vec![extension_group_button(preferences, ExtensionGroup::Other)],
        vec![pref_button("Back", "pref:main")],
    ]
}

/// Builds the concrete extension selector keyboard.
fn extension_group_keyboard(
    preferences: &Preferences,
    group: ExtensionGroup,
) -> Vec<Vec<InlineKeyboardButton>> {
    let mut rows = group
        .extensions()
        .chunks(3)
        .map(|chunk| {
            chunk
                .iter()
                .map(|ext| {
                    pref_button(
                        checked_label(preferences.extension.as_deref() == Some(*ext), ext),
                        format!("pref:ext:set:{ext}"),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    rows.push(vec![pref_button("Clear extension", "pref:ext:off")]);
    rows.push(vec![
        pref_button("Back", "pref:ext"),
        pref_button("Main", "pref:main"),
    ]);
    rows
}

/// Builds a group button with a check mark when the current extension belongs to it.
fn extension_group_button(
    preferences: &Preferences,
    group: ExtensionGroup,
) -> InlineKeyboardButton {
    let selected = preferences
        .extension
        .as_deref()
        .is_some_and(|ext| group.extensions().contains(&ext));
    pref_button(
        checked_label(selected, group.label()),
        format!("pref:ext:group:{}", group.callback_value()),
    )
}

/// Creates one preference keyboard button.
fn pref_button(text: impl Into<String>, callback_data: impl Into<String>) -> InlineKeyboardButton {
    InlineKeyboardButton {
        text: text.into(),
        callback_data: Some(callback_data.into()),
        url: None,
        style: None,
    }
}

/// Returns a check-mark label for exclusive choices.
fn checked_label(selected: bool, label: &str) -> String {
    if selected {
        format!("✅ {label}")
    } else {
        label.to_string()
    }
}

/// Returns a check-mark label for boolean toggles.
fn toggle_label(enabled: bool, label: &str) -> String {
    if enabled {
        format!("✅ {label}")
    } else {
        label.to_string()
    }
}

/// Returns the extension group that owns an extension.
fn extension_group_for(ext: &str) -> Option<ExtensionGroup> {
    [
        ExtensionGroup::Images,
        ExtensionGroup::Audio,
        ExtensionGroup::Video,
        ExtensionGroup::Documents,
        ExtensionGroup::Other,
    ]
    .into_iter()
    .find(|group| group.extensions().contains(&ext))
}

/// Verifies Telegram's webhook secret header when configured.
fn verify_telegram_secret(config: &Config, request: &Request) -> Result<()> {
    let Some(expected) = &config.telegram_webhook_secret else {
        return Ok(());
    };
    let actual = request
        .headers()
        .get("x-telegram-bot-api-secret-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    anyhow::ensure!(actual == expected, "invalid Telegram webhook secret");
    Ok(())
}

/// Applies a `/prefs ...` command to the current preferences.
pub fn update_preferences(text: &str, mut preferences: Preferences) -> Preferences {
    let tokens = tokenize(text);
    let args = tokens
        .iter()
        .skip(1)
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if args.len() >= 2 {
        match (args[0].as_str(), args[1].as_str()) {
            ("category-counts", "on") => preferences.show_category_counts = true,
            ("category-counts", "off") => preferences.show_category_counts = false,
            ("sha1", "on") => preferences.show_sha1 = true,
            ("sha1", "off") => preferences.show_sha1 = false,
            ("filesize", "on") => preferences.show_file_size = true,
            ("filesize", "off") => preferences.show_file_size = false,
            ("preview-metadata" | "metadata", "on") => preferences.show_preview_metadata = true,
            ("preview-metadata" | "metadata", "off") => preferences.show_preview_metadata = false,
            ("rich-previews" | "rich", "on") => preferences.rich_image_previews = true,
            ("rich-previews" | "rich", "off") => preferences.rich_image_previews = false,
            ("category-file-buttons" | "category-files", "on" | "buttons") => {
                preferences.category_file_buttons = true;
            }
            ("category-file-buttons" | "category-files", "off" | "images") => {
                preferences.category_file_buttons = false;
            }
            ("pagination", "on") => preferences.pagination_enabled = true,
            ("pagination", "off") => preferences.pagination_enabled = false,
            ("inline" | "inline-count", value) => {
                if let Ok(count) = value.parse::<usize>() {
                    preferences.inline_result_count = normalize_inline_result_count(count);
                }
            }
            ("mode", value) => {
                if let Some(mode) = DeliveryMode::parse(value) {
                    preferences.delivery_mode = mode;
                }
            }
            ("type", value) => {
                if let Some(file_type) = FileType::parse(value) {
                    preferences.file_type = file_type;
                }
            }
            ("ext", "off") => preferences.extension = None,
            ("ext", value) => {
                let extension = value.trim_start_matches('.');
                if is_commons_supported_extension(extension) {
                    preferences.extension = Some(extension.into());
                }
            }
            ("favorite", "add") if args.len() >= 3 => {
                preferences.favorite_categories.push(tokens[3..].join(" "));
            }
            ("favorite", "remove") if args.len() >= 3 => {
                let value = tokens[3..].join(" ");
                preferences
                    .favorite_categories
                    .retain(|item| item != &value);
            }
            ("blacklist-category", "add") if args.len() >= 3 => {
                preferences.blacklist_categories.push(tokens[3..].join(" "));
            }
            ("blacklist-category", "remove") if args.len() >= 3 => {
                let value = tokens[3..].join(" ");
                preferences
                    .blacklist_categories
                    .retain(|item| item != &value);
            }
            ("blacklist-user", "add") if args.len() >= 3 => {
                preferences.blacklist_uploaders.push(tokens[3..].join(" "));
            }
            ("blacklist-user", "remove") if args.len() >= 3 => {
                let value = tokens[3..].join(" ");
                preferences
                    .blacklist_uploaders
                    .retain(|item| item != &value);
            }
            _ => {}
        }
    }
    preferences
}

/// Formats the current preferences for Telegram.
fn format_preferences(preferences: &Preferences, config: &Config) -> String {
    let storage = if config.stateless_mode {
        "stateless mode, not saved"
    } else {
        "saved in DynamoDB when configured"
    };
    format!(
        "Preferences ({storage})\n\nCategory counts: {}\nMode: {}\nFile type: {}\nExtension: {}\nPreview metadata: {}\nRich previews: {}\nCategory file buttons: {}\nPagination: {}\nInline results: {}\nSHA-1: {}\nFile size in buttons: {}\nFavorites: {}\nCategory blacklist: {}\nUploader blacklist: {}\n\nUse the buttons below, or commands:\n/settings mode buttons|links10|images10|images20\n/settings type all|images|audio|video\n/settings ext jpg|webp|flac|pdf|off\n/settings category-counts on|off\n/settings preview-metadata on|off\n/settings rich-previews on|off\n/settings category-file-buttons on|off\n/settings pagination on|off\n/settings inline 10|20|50\n/settings sha1 on|off\n/settings filesize on|off\n/settings favorite add Category name\n/settings blacklist-category add Category name\n/settings blacklist-user add Username\n\nAliases: /prefs, /preferences",
        yes_no(preferences.show_category_counts),
        preferences.delivery_mode.as_pref_value(),
        preferences.file_type.as_pref_value(),
        preferences.extension.as_deref().unwrap_or("none"),
        yes_no(preferences.show_preview_metadata),
        yes_no(preferences.rich_image_previews),
        yes_no(preferences.category_file_buttons),
        yes_no(preferences.pagination_enabled),
        preferences.normalized_inline_result_count(),
        yes_no(preferences.show_sha1),
        yes_no(preferences.show_file_size),
        preferences.favorite_categories.join(", "),
        preferences.blacklist_categories.join(", "),
        preferences.blacklist_uploaders.join(", "),
    )
}

/// Returns the `/help` text.
fn help_text(config: &Config, preferences: &Preferences) -> String {
    let favorites = if preferences.favorite_categories.is_empty() {
        String::new()
    } else {
        format!(
            "\nFavorite categories: {}",
            preferences.favorite_categories.join(", ")
        )
    };
    format!(
        "<b>Wikimedia Commons bot</b>\n\nUnofficial Wikimedia Commons search bot. Source: <a href=\"{}\">{}</a>\nLicense: MIT\n\nSearch examples:\n<pre>Minsk\n-img Minsk\n-links Minsk\nimage Minsk\nbild Berlin\naudio:bird\nflac Minsk c birds\nUser:Vitaly_Zdanevich date:2025 Minsk\nuser:Vitaly_Zdanevich d:7days audio:something\ns:&gt;10MB s:&lt;20MB Minsk\nCategory Minsk\nKategorie Berlin\nКатегория Минск\nКатэгорыя Мінск</pre>\n\nAliases: category/c, Kategorie/kat/k, категория/катэгорыя/к/с; user/u/Benutzer; image/images/img/i, Bild/Bilder/Foto/Fotos, выява/ваява/в; audio/sound/music, Ton/Klang/Musik, аудио/музыка/звук/аудыё; date/d/Datum; size/s/Größe/Groesse/g. Use -img for Telegram image previews with metadata captions, -links for 10 compact links, and /settings to open preferences. Preview metadata and button pagination are enabled by default and can be disabled in /settings. Rich previews can be enabled in /settings to send one Telegram rich message with photos and text together.\n\nCategory buttons show category info first, then up to 20 direct image previews with captions, then subcategories. Filename buttons for category files can be enabled in /settings.\n\nSend your current Telegram location to get up to 20 closest Wikimedia Commons category buttons. Inline mode can use Telegram's shared location to show nearby geotagged images. Without location, or with structured filters like user:, category, date, size, or extension, it searches by your typed query. Inline result count can be set in /settings to 10, 20, or 50 for slower networks.\n\nClick file buttons to receive the file with metadata, license, uploader, date, source link, geolocation when available, and upload version count. Telegram bot uploads are limited to 50 MB, so larger files are filtered out. Files larger than 20 MB use red buttons; audio uses blue buttons.\n\nUpload your own free photos, audio, video, and other files to Wikimedia Commons: <a href=\"https://commons.wikimedia.org/wiki/Special:UploadWizard\">Upload Wizard</a>. Many upload tools are listed at <a href=\"https://commons.wikimedia.org/wiki/Commons:Upload_tools\">Commons upload tools</a>. Storage is unlimited, and all files are public.\n\nSupport: @vitaly_zdanevich\n\nAWS free-tier docs: <a href=\"https://aws.amazon.com/lambda/pricing/\">Lambda</a>, <a href=\"https://aws.amazon.com/dynamodb/pricing/\">DynamoDB</a>.{favorites}",
        config.github_url, config.github_url
    )
}

/// Enriches category buttons with file counts when enabled.
async fn enrich_category_counts(
    commons: &CommonsClient,
    categories: &mut [crate::models::CategoryHit],
) {
    for category in categories.iter_mut() {
        if let Ok(count) = commons.category_file_count(&category.display_title).await {
            category.file_count = Some(count);
        }
    }
}

/// Returns "yes" or "no" for booleans.
fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::{
        ExtensionGroup, PreferencesView, apply_preference_callback, category_file_result_limit,
        category_result_limit, checked_label, extension_group_for, file_result_limit,
        format_preferences, help_text, idempotency_keys, inline_location_applies,
        inline_lookup_limit, inline_result_page, is_expired_inline_query_error,
        is_preferences_update_command, parse_inline_offset, parse_pagination_callback,
        preferences_keyboard, toggle_label, update_preferences,
    };
    use crate::config::Config;
    use crate::models::{
        DeliveryMode, DocumentPageMode, FileHit, FileType, Preferences, SearchQuery, SizeFilter,
        SizeOp,
    };
    use crate::telegram::{CallbackQuery, Chat, Message, Update, User};

    fn test_config(stateless_mode: bool) -> Config {
        Config {
            telegram_bot_token: None,
            telegram_webhook_secret: None,
            admin_user_ids: vec![42],
            github_url: "https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons".into(),
            aws_region: "us-east-1".into(),
            lambda_function_name: "telegram-wikimedia-commons-bot".into(),
            dynamodb_table: Some("telegram-wikimedia-commons-bot-preferences".into()),
            stateless_mode,
            max_file_bytes: 50 * 1024 * 1024,
            user_agent: "test-agent".into(),
            commons_api_url: "https://commons.wikimedia.org/w/api.php".into(),
            commons_auth_cookie_ssm_parameter: None,
            enable_test_endpoint: false,
        }
    }

    fn keyboard_labels(keyboard: &crate::telegram::InlineKeyboardMarkup) -> Vec<&str> {
        keyboard
            .inline_keyboard
            .iter()
            .flatten()
            .map(|button| button.text.as_str())
            .collect()
    }

    #[test]
    fn updates_preferences_from_command() {
        let prefs = update_preferences("/prefs mode images20", Preferences::default());
        assert_eq!(prefs.delivery_mode, DeliveryMode::Images20);
        let prefs = update_preferences("/prefs type audio", prefs);
        assert_eq!(prefs.file_type, FileType::Audio);
        let prefs = update_preferences("/prefs sha1 on", prefs);
        assert!(prefs.show_sha1);
        let prefs = update_preferences("/prefs preview-metadata off", prefs);
        assert!(!prefs.show_preview_metadata);
        let prefs = update_preferences("/prefs rich-previews on", prefs);
        assert!(prefs.rich_image_previews);
        let prefs = update_preferences("/prefs category-file-buttons on", prefs);
        assert!(prefs.category_file_buttons);
        let prefs = update_preferences("/prefs pagination off", prefs);
        assert!(!prefs.pagination_enabled);
        let prefs = update_preferences("/prefs inline 10", prefs);
        assert_eq!(prefs.inline_result_count, 10);
        let prefs = update_preferences("/settings ext webp", prefs);
        assert_eq!(prefs.extension, Some("webp".into()));
        let prefs = update_preferences("/settings ext avif", prefs);
        assert_eq!(prefs.extension, Some("webp".into()));
    }

    #[test]
    fn updates_list_preferences_from_commands() {
        let prefs = update_preferences(
            "/settings favorite add Belarusian wooden churches",
            Preferences::default(),
        );
        assert_eq!(
            prefs.favorite_categories,
            vec!["Belarusian wooden churches"]
        );
        let prefs = update_preferences(
            "/settings favorite remove Belarusian wooden churches",
            prefs,
        );
        assert!(prefs.favorite_categories.is_empty());

        let prefs = update_preferences(
            "/settings blacklist-category add Bad scans",
            Preferences::default(),
        );
        assert_eq!(prefs.blacklist_categories, vec!["Bad scans"]);
        let prefs = update_preferences("/settings blacklist-category remove Bad scans", prefs);
        assert!(prefs.blacklist_categories.is_empty());

        let prefs = update_preferences(
            "/settings blacklist-user add Example_User",
            Preferences::default(),
        );
        assert_eq!(prefs.blacklist_uploaders, vec!["Example_User"]);
        let prefs = update_preferences("/settings blacklist-user remove Example_User", prefs);
        assert!(prefs.blacklist_uploaders.is_empty());

        let prefs = update_preferences("/settings category-counts on", Preferences::default());
        assert!(prefs.show_category_counts);
        let prefs = update_preferences("/settings filesize on", prefs);
        assert!(prefs.show_file_size);
        let prefs = update_preferences("/settings metadata off", prefs);
        assert!(!prefs.show_preview_metadata);
        let prefs = update_preferences("/settings rich off", prefs);
        assert!(!prefs.rich_image_previews);
        let prefs = update_preferences("/settings category-files images", prefs);
        assert!(!prefs.category_file_buttons);

        let prefs = update_preferences(
            "/settings ext off",
            Preferences {
                extension: Some("jpg".into()),
                ..Preferences::default()
            },
        );
        assert_eq!(prefs.extension, None);
    }

    #[test]
    fn recognizes_settings_update_command() {
        assert!(is_preferences_update_command("/settings mode links10"));
        assert!(is_preferences_update_command("/prefs mode links10"));
        assert!(!is_preferences_update_command("/settings"));
    }

    #[test]
    fn builds_idempotency_keys_for_file_callbacks() {
        let update = Update {
            update_id: Some(123),
            message: None,
            callback_query: Some(CallbackQuery {
                id: "callback-id".into(),
                from: User { id: 42 },
                message: Some(Message {
                    message_id: Some(7),
                    chat: Chat { id: -100 },
                    from: None,
                    text: None,
                    location: None,
                }),
                data: Some("file:99".into()),
            }),
            inline_query: None,
        };

        let keys = idempotency_keys(&update);

        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key, "TELEGRAM_UPDATE#123");
        assert_eq!(keys[1].key, "TELEGRAM_CALLBACK_ACTION#42#-100#7#file:99");
    }

    #[test]
    fn skips_action_idempotency_for_non_file_callbacks() {
        let update = Update {
            update_id: Some(124),
            message: None,
            callback_query: Some(CallbackQuery {
                id: "callback-id".into(),
                from: User { id: 42 },
                message: Some(Message {
                    message_id: Some(7),
                    chat: Chat { id: -100 },
                    from: None,
                    text: None,
                    location: None,
                }),
                data: Some("pg:f:token:1".into()),
            }),
            inline_query: None,
        };

        let keys = idempotency_keys(&update);

        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key, "TELEGRAM_UPDATE#124");
    }

    #[test]
    fn help_formats_search_examples_as_code() {
        let text = help_text(&test_config(false), &Preferences::default());
        assert!(
            text.contains("https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons")
        );
        assert!(text.contains("License: MIT"));
        assert!(text.contains("Support: @vitaly_zdanevich"));
        assert!(text.contains("Search examples:\n<pre>Minsk"));
        assert!(text.contains("Катэгорыя Мінск</pre>"));
        assert!(text.contains("Rich previews can be enabled in /settings"));
        assert!(text.contains("Category buttons show category info first"));
        assert!(text.contains("Send your current Telegram location"));
    }

    #[test]
    fn help_lists_favorite_categories_when_configured() {
        let text = help_text(
            &test_config(false),
            &Preferences {
                favorite_categories: vec!["Minsk".into(), "Brest Fortress".into()],
                ..Preferences::default()
            },
        );

        assert!(text.contains("Favorite categories: Minsk, Brest Fortress"));
    }

    #[test]
    fn inline_location_applies_to_plain_image_searches() {
        assert!(inline_location_applies(&SearchQuery::default()));
        assert!(inline_location_applies(&SearchQuery {
            terms: vec!["Minsk".into()],
            file_type: Some(FileType::Images),
            ..SearchQuery::default()
        }));
    }

    #[test]
    fn inline_location_does_not_override_structured_searches() {
        assert!(!inline_location_applies(&SearchQuery {
            user: Some("Vitaly_Zdanevich".into()),
            file_type: Some(FileType::Images),
            ..SearchQuery::default()
        }));
        assert!(!inline_location_applies(&SearchQuery {
            extension: Some("jpg".into()),
            file_type: Some(FileType::Images),
            ..SearchQuery::default()
        }));
        assert!(!inline_location_applies(&SearchQuery {
            size_filters: vec![SizeFilter {
                op: SizeOp::LessThan,
                bytes: 1024,
            }],
            file_type: Some(FileType::Images),
            ..SearchQuery::default()
        }));
        assert!(!inline_location_applies(&SearchQuery {
            terms: vec!["bird".into()],
            file_type: Some(FileType::Audio),
            ..SearchQuery::default()
        }));
    }

    #[test]
    fn detects_expired_inline_query_errors() {
        let error = anyhow::anyhow!(
            "Telegram method answerInlineQuery failed with HTTP 400 Bad Request: query is too old and response timeout expired or query ID is invalid"
        );

        assert!(is_expired_inline_query_error(&error));
        assert!(!is_expired_inline_query_error(&anyhow::anyhow!(
            "Telegram method answerInlineQuery failed with HTTP 500"
        )));
    }

    #[test]
    fn paginates_inline_results_with_next_offset() {
        let files = (0..101)
            .map(|page_id| FileHit {
                page_id,
                file_name: format!("{page_id}.jpg"),
                ..FileHit::default()
            })
            .collect::<Vec<_>>();

        let (page, next_offset) = inline_result_page(files, 50, 50);

        assert_eq!(page.len(), 50);
        assert_eq!(page[0].page_id, 50);
        assert_eq!(next_offset.as_deref(), Some("100"));
    }

    #[test]
    fn paginates_inline_results_with_smaller_preference() {
        let files = (0..25)
            .map(|page_id| FileHit {
                page_id,
                file_name: format!("{page_id}.jpg"),
                ..FileHit::default()
            })
            .collect::<Vec<_>>();

        let (page, next_offset) = inline_result_page(files, 0, 10);

        assert_eq!(page.len(), 10);
        assert_eq!(page[9].page_id, 9);
        assert_eq!(next_offset.as_deref(), Some("10"));
    }

    #[test]
    fn parses_and_caps_inline_offsets() {
        assert_eq!(parse_inline_offset("50"), 50);
        assert_eq!(parse_inline_offset("9999"), 0);
        assert_eq!(parse_inline_offset("not-a-number"), 0);
        assert_eq!(inline_lookup_limit(0, 50), 51);
        assert_eq!(inline_lookup_limit(100, 50), 151);
        assert_eq!(inline_lookup_limit(0, 10), 11);
    }

    #[test]
    fn builds_preferences_keyboard_with_checks() {
        let prefs = Preferences {
            delivery_mode: DeliveryMode::Links10,
            file_type: FileType::Images,
            show_sha1: true,
            extension: Some("webp".into()),
            ..Preferences::default()
        };
        let keyboard = preferences_keyboard(&prefs, PreferencesView::Main);
        let labels = keyboard_labels(&keyboard);
        assert!(labels.contains(&"✅ Links10"));
        assert!(labels.contains(&"✅ Images"));
        assert!(labels.contains(&"✅ SHA-1"));
        assert!(labels.contains(&"✅ Preview metadata"));
        assert!(labels.contains(&"Category file buttons"));
        assert!(labels.contains(&"✅ Pagination"));
        assert!(labels.contains(&"✅ Inline 50"));
        assert!(labels.contains(&"Extension: webp"));
    }

    #[test]
    fn renders_extension_keyboards_with_group_checks() {
        let prefs = Preferences {
            extension: Some("flac".into()),
            ..Preferences::default()
        };

        let menu = preferences_keyboard(&prefs, PreferencesView::ExtensionMenu);
        let menu_labels = keyboard_labels(&menu);
        assert!(menu_labels.contains(&"All extensions"));
        assert!(menu_labels.contains(&"Images"));
        assert!(menu_labels.contains(&"✅ Audio"));
        assert!(menu_labels.contains(&"Documents"));
        assert!(menu_labels.contains(&"Back"));

        let group = preferences_keyboard(
            &prefs,
            PreferencesView::ExtensionGroup(ExtensionGroup::Audio),
        );
        let group_labels = keyboard_labels(&group);
        assert!(group_labels.contains(&"✅ flac"));
        assert!(group_labels.contains(&"Clear extension"));
        assert!(group_labels.contains(&"Main"));
    }

    #[test]
    fn applies_extension_callback() {
        let result = apply_preference_callback("ext:set:jpg", Preferences::default()).unwrap();
        assert!(result.changed);
        assert_eq!(result.preferences.extension, Some("jpg".into()));
        assert_eq!(
            result.view,
            PreferencesView::ExtensionGroup(ExtensionGroup::Images)
        );
        assert!(apply_preference_callback("ext:set:avif", Preferences::default()).is_none());
    }

    #[test]
    fn extension_groups_map_supported_extensions() {
        assert_eq!(extension_group_for("jpg"), Some(ExtensionGroup::Images));
        assert_eq!(extension_group_for("flac"), Some(ExtensionGroup::Audio));
        assert_eq!(extension_group_for("webm"), Some(ExtensionGroup::Video));
        assert_eq!(extension_group_for("pdf"), Some(ExtensionGroup::Documents));
        assert_eq!(extension_group_for("stl"), Some(ExtensionGroup::Other));
        assert_eq!(extension_group_for("avif"), None);
    }

    #[test]
    fn toggles_preview_metadata_callback() {
        let result =
            apply_preference_callback("toggle:preview-metadata", Preferences::default()).unwrap();
        assert!(result.changed);
        assert!(!result.preferences.show_preview_metadata);
    }

    #[test]
    fn toggles_rich_previews_callback() {
        let result =
            apply_preference_callback("toggle:rich-previews", Preferences::default()).unwrap();
        assert!(result.changed);
        assert!(result.preferences.rich_image_previews);
    }

    #[test]
    fn toggles_category_file_buttons_callback() {
        let result =
            apply_preference_callback("toggle:category-file-buttons", Preferences::default())
                .unwrap();
        assert!(result.changed);
        assert!(result.preferences.category_file_buttons);
    }

    #[test]
    fn toggles_pagination_callback() {
        let result =
            apply_preference_callback("toggle:pagination", Preferences::default()).unwrap();

        assert!(result.changed);
        assert!(!result.preferences.pagination_enabled);
    }

    #[test]
    fn applies_more_preference_callbacks() {
        let result =
            apply_preference_callback("toggle:category-counts", Preferences::default()).unwrap();
        assert!(result.changed);
        assert!(result.preferences.show_category_counts);

        let result = apply_preference_callback("toggle:sha1", Preferences::default()).unwrap();
        assert!(result.changed);
        assert!(result.preferences.show_sha1);

        let result = apply_preference_callback("toggle:filesize", Preferences::default()).unwrap();
        assert!(result.changed);
        assert!(result.preferences.show_file_size);

        let result = apply_preference_callback("mode:images10", Preferences::default()).unwrap();
        assert_eq!(result.preferences.delivery_mode, DeliveryMode::Images10);

        let result = apply_preference_callback("type:video", Preferences::default()).unwrap();
        assert_eq!(result.preferences.file_type, FileType::Video);

        let result = apply_preference_callback("pdf:rendered", Preferences::default()).unwrap();
        assert_eq!(result.preferences.pdf_mode, DocumentPageMode::RenderedPages);

        let result = apply_preference_callback("djvu:rendered", Preferences::default()).unwrap();
        assert_eq!(
            result.preferences.djvu_mode,
            DocumentPageMode::RenderedPages
        );

        let result =
            apply_preference_callback("ext:group:documents", Preferences::default()).unwrap();
        assert!(!result.changed);
        assert_eq!(
            result.view,
            PreferencesView::ExtensionGroup(ExtensionGroup::Documents)
        );

        let result = apply_preference_callback(
            "ext:set:jpg",
            Preferences {
                extension: Some("jpg".into()),
                ..Preferences::default()
            },
        )
        .unwrap();
        assert!(!result.changed);
        assert!(!result.render);

        assert!(apply_preference_callback("mode:bad", Preferences::default()).is_none());
    }

    #[test]
    fn applies_inline_count_callback() {
        let result = apply_preference_callback("inline:10", Preferences::default()).unwrap();

        assert!(result.changed);
        assert_eq!(result.preferences.inline_result_count, 10);
    }

    #[test]
    fn noops_preference_callback_when_value_is_already_selected() {
        let result = apply_preference_callback(
            "inline:20",
            Preferences {
                inline_result_count: 20,
                ..Preferences::default()
            },
        )
        .unwrap();
        assert!(!result.changed);
        assert!(!result.render);

        let result = apply_preference_callback("ext:off", Preferences::default()).unwrap();
        assert!(!result.changed);
        assert!(!result.render);
        assert_eq!(result.view, PreferencesView::ExtensionMenu);
    }

    #[test]
    fn parses_pagination_callback_payload() {
        assert_eq!(
            parse_pagination_callback("pg:f:abcdef0123456789:2"),
            Some(("f", "abcdef0123456789", 2))
        );
        assert!(parse_pagination_callback("file:1").is_none());
    }

    #[test]
    fn widens_only_paginated_button_result_limits() {
        let prefs = Preferences::default();
        assert_eq!(file_result_limit(&SearchQuery::default(), &prefs), 60);
        assert_eq!(category_result_limit(&prefs), 60);
        assert_eq!(category_file_result_limit(&prefs), 20);

        let image_query = SearchQuery {
            image_previews_flag: true,
            ..SearchQuery::default()
        };
        assert_eq!(file_result_limit(&image_query, &prefs), 20);

        let no_pagination = Preferences {
            pagination_enabled: false,
            ..Preferences::default()
        };
        assert_eq!(
            file_result_limit(&SearchQuery::default(), &no_pagination),
            20
        );
        assert_eq!(category_result_limit(&no_pagination), 20);

        let category_file_buttons = Preferences {
            category_file_buttons: true,
            ..Preferences::default()
        };
        assert_eq!(category_file_result_limit(&category_file_buttons), 60);
    }

    #[test]
    fn formats_preferences_with_storage_lists_and_commands() {
        let text = format_preferences(
            &Preferences {
                show_category_counts: true,
                delivery_mode: DeliveryMode::Images20,
                file_type: FileType::Audio,
                extension: Some("flac".into()),
                favorite_categories: vec!["Minsk".into()],
                blacklist_categories: vec!["Low quality scans".into()],
                blacklist_uploaders: vec!["Example_User".into()],
                show_sha1: true,
                show_file_size: true,
                inline_result_count: 10,
                ..Preferences::default()
            },
            &test_config(true),
        );

        assert!(text.contains("Preferences (stateless mode, not saved)"));
        assert!(text.contains("Mode: images20"));
        assert!(text.contains("File type: audio"));
        assert!(text.contains("Extension: flac"));
        assert!(text.contains("Rich previews: no"));
        assert!(text.contains("Category file buttons: no"));
        assert!(text.contains("Inline results: 10"));
        assert!(text.contains("Favorites: Minsk"));
        assert!(text.contains("Category blacklist: Low quality scans"));
        assert!(text.contains("Uploader blacklist: Example_User"));
        assert!(text.contains("/settings inline 10|20|50"));
        assert!(text.contains("/settings rich-previews on|off"));
        assert!(text.contains("/settings category-file-buttons on|off"));
    }

    #[test]
    fn labels_show_checks_only_when_selected() {
        assert_eq!(checked_label(true, "Images"), "✅ Images");
        assert_eq!(checked_label(false, "Images"), "Images");
        assert_eq!(toggle_label(true, "Pagination"), "✅ Pagination");
        assert_eq!(toggle_label(false, "Pagination"), "Pagination");
    }
}
