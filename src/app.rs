use crate::commons::CommonsClient;
use crate::config::Config;
use crate::models::{
    COMMONS_AUDIO_EXTENSIONS, COMMONS_DOCUMENT_EXTENSIONS, COMMONS_IMAGE_EXTENSIONS,
    COMMONS_MODEL_EXTENSIONS, COMMONS_VIDEO_EXTENSIONS, DeliveryMode, DocumentPageMode, FileType,
    Intent, Preferences, SearchQuery,
};
use crate::parser::{is_commons_supported_extension, parse_intent, tokenize};
use crate::preferences::PreferenceStore;
use crate::stats::load_admin_stats;
use crate::telegram::{
    InlineKeyboardButton, InlineKeyboardMarkup, TelegramClient, Update, format_category_info,
    format_file_metadata, send_search_results,
};
use crate::wikidata::WikidataClient;
use anyhow::{Context, Result};
use lambda_http::{Body, Request, Response};
use serde_json::json;

/// Handles one AWS Lambda HTTP request from Telegram.
pub async fn handle_lambda_request(request: Request) -> Result<Response<Body>> {
    let config = Config::from_env();
    verify_telegram_secret(&config, &request)?;
    if config.enable_test_endpoint && request.uri().path() == "/__test" {
        return handle_test_endpoint(&request);
    }
    let update: Update = serde_json::from_slice(request.body().as_ref())?;
    handle_update(update, &config).await?;
    Ok(Response::builder()
        .status(200)
        .body(Body::Text("ok".into()))?)
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
        let text = message.text.unwrap_or_default();
        telegram.send_typing(chat_id).await.ok();
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
                let mut categories = commons.search_categories(&query, 20).await?;
                let prefs = preferences.get(user_id).await;
                if prefs.show_category_counts {
                    enrich_category_counts(&commons, &mut categories).await;
                }
                telegram.send_category_buttons(chat_id, &categories).await?;
            }
            Intent::FileSearch(query) => {
                let prefs = preferences.get(user_id).await;
                let files = commons
                    .search_files(&query, &prefs, 20, config.max_file_bytes)
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
                        let mut categories = commons.search_categories(&category_query, 20).await?;
                        if prefs.show_category_counts {
                            enrich_category_counts(&commons, &mut categories).await;
                        }
                        telegram.send_category_buttons(chat_id, &categories).await?;
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
                let mut category = commons
                    .category_info_by_page_id(page_id, 20, 20, config.max_file_bytes)
                    .await?;
                enrich_category_wikidata(&wikidata, &mut category).await;
                telegram
                    .send_message(chat_id, &format_category_info(&category), None)
                    .await?;
                telegram
                    .send_file_buttons(chat_id, &category.files, &prefs)
                    .await?;
                telegram
                    .send_subcategory_buttons(chat_id, &category.subcategories)
                    .await?;
            }
        }
    }

    if let Some(inline_query) = update.inline_query {
        let prefs = preferences.get(inline_query.from.id).await;
        let location = inline_query.location;
        let mut parsed = match parse_intent(&inline_query.query) {
            Intent::FileSearch(query) => query,
            _ => Default::default(),
        };
        if parsed.file_type.is_none() {
            parsed.file_type = Some(FileType::Images);
        }
        let use_location = location.is_some() && inline_location_applies(&parsed);
        let files = if let (Some(location), true) = (location, use_location) {
            commons
                .search_nearby_files(
                    location.latitude,
                    location.longitude,
                    &parsed,
                    &prefs,
                    20,
                    config.max_file_bytes,
                )
                .await?
        } else {
            commons
                .search_files(&parsed, &prefs, 20, config.max_file_bytes)
                .await?
        };
        telegram
            .answer_inline_query(&inline_query.id, &files, use_location)
            .await?;
    }

    Ok(())
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
                toggle_label(preferences.show_sha1, "SHA-1"),
                "pref:toggle:sha1",
            ),
        ],
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
        "Preferences ({storage})\n\nCategory counts: {}\nMode: {}\nFile type: {}\nExtension: {}\nPreview metadata: {}\nSHA-1: {}\nFile size in buttons: {}\nFavorites: {}\nCategory blacklist: {}\nUploader blacklist: {}\n\nUse the buttons below, or commands:\n/settings mode buttons|links10|images10|images20\n/settings type all|images|audio|video\n/settings ext jpg|webp|flac|pdf|off\n/settings category-counts on|off\n/settings preview-metadata on|off\n/settings sha1 on|off\n/settings filesize on|off\n/settings favorite add Category name\n/settings blacklist-category add Category name\n/settings blacklist-user add Username\n\nAliases: /prefs, /preferences",
        yes_no(preferences.show_category_counts),
        preferences.delivery_mode.as_pref_value(),
        preferences.file_type.as_pref_value(),
        preferences.extension.as_deref().unwrap_or("none"),
        yes_no(preferences.show_preview_metadata),
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
        "<b>Wikimedia Commons bot</b>\n\nUnofficial Wikimedia Commons search bot. Source: <a href=\"{}\">{}</a>\nLicense: MIT\n\nSearch examples:\n<pre>Minsk\n-img Minsk\n-links Minsk\nimage Minsk\nbild Berlin\naudio:bird\nflac Minsk c birds\nUser:Vitaly_Zdanevich date:2025 Minsk\nuser:Vitaly_Zdanevich d:7days audio:something\ns:&gt;10MB s:&lt;20MB Minsk\nCategory Minsk\nKategorie Berlin\nКатегория Минск\nКатэгорыя Мінск</pre>\n\nAliases: category/c, Kategorie/kat/k, категория/катэгорыя/к/с; user/u/Benutzer; image/images/img/i, Bild/Bilder/Foto/Fotos, выява/ваява/в; audio/sound/music, Ton/Klang/Musik, аудио/музыка/звук/аудыё; date/d/Datum; size/s/Größe/Groesse/g. Use -img for Telegram image previews with metadata captions, -links for 10 compact links, and /settings to open preferences. Preview metadata is enabled by default and can be disabled in /settings.\n\nInline mode can use Telegram's shared location to show nearby geotagged images. Without location, or with structured filters like user:, category, date, size, or extension, it searches by your typed query.\n\nClick file buttons to receive the file with metadata, license, uploader, date, source link, geolocation when available, and upload version count. Telegram bot uploads are limited to 50 MB, so larger files are filtered out. Files larger than 20 MB use red buttons; audio uses blue buttons.\n\nUpload your own free photos, audio, video, and other files to Wikimedia Commons: <a href=\"https://commons.wikimedia.org/wiki/Special:UploadWizard\">Upload Wizard</a>. Many upload tools are listed at <a href=\"https://commons.wikimedia.org/wiki/Commons:Upload_tools\">Commons upload tools</a>. Storage is unlimited, and all files are public.\n\nSupport: @vitaly_zdanevich\n\nAWS free-tier docs: <a href=\"https://aws.amazon.com/lambda/pricing/\">Lambda</a>, <a href=\"https://aws.amazon.com/dynamodb/pricing/\">DynamoDB</a>.{favorites}",
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
        ExtensionGroup, PreferencesView, apply_preference_callback, help_text,
        inline_location_applies, is_preferences_update_command, preferences_keyboard,
        update_preferences,
    };
    use crate::config::Config;
    use crate::models::{DeliveryMode, FileType, Preferences, SearchQuery, SizeFilter, SizeOp};

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
        let prefs = update_preferences("/settings ext webp", prefs);
        assert_eq!(prefs.extension, Some("webp".into()));
        let prefs = update_preferences("/settings ext avif", prefs);
        assert_eq!(prefs.extension, Some("webp".into()));
    }

    #[test]
    fn recognizes_settings_update_command() {
        assert!(is_preferences_update_command("/settings mode links10"));
        assert!(is_preferences_update_command("/prefs mode links10"));
        assert!(!is_preferences_update_command("/settings"));
    }

    #[test]
    fn help_formats_search_examples_as_code() {
        let text = help_text(&Config::from_env(), &Preferences::default());
        assert!(
            text.contains("https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons")
        );
        assert!(text.contains("License: MIT"));
        assert!(text.contains("Support: @vitaly_zdanevich"));
        assert!(text.contains("Search examples:\n<pre>Minsk"));
        assert!(text.contains("Катэгорыя Мінск</pre>"));
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
    fn builds_preferences_keyboard_with_checks() {
        let prefs = Preferences {
            delivery_mode: DeliveryMode::Links10,
            file_type: FileType::Images,
            show_sha1: true,
            extension: Some("webp".into()),
            ..Preferences::default()
        };
        let keyboard = preferences_keyboard(&prefs, PreferencesView::Main);
        let labels = keyboard
            .inline_keyboard
            .iter()
            .flatten()
            .map(|button| button.text.as_str())
            .collect::<Vec<_>>();
        assert!(labels.contains(&"✅ Links10"));
        assert!(labels.contains(&"✅ Images"));
        assert!(labels.contains(&"✅ SHA-1"));
        assert!(labels.contains(&"✅ Preview metadata"));
        assert!(labels.contains(&"Extension: webp"));
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
    fn toggles_preview_metadata_callback() {
        let result =
            apply_preference_callback("toggle:preview-metadata", Preferences::default()).unwrap();
        assert!(result.changed);
        assert!(!result.preferences.show_preview_metadata);
    }
}
