use crate::commons::CommonsClient;
use crate::models::{
    CategoryHit, CategoryInfo, DeliveryMode, FileHit, Preferences, TELEGRAM_REMOTE_URL_LIMIT_BYTES,
};
use crate::pagination::{BUTTON_PAGE_SIZE, page_count, store_category_list, store_file_list};
use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, multipart};
use serde::{Deserialize, Serialize};

/// Telegram's documented maximum photo size when Telegram fetches a URL.
const TELEGRAM_PHOTO_URL_LIMIT_BYTES: u64 = 5 * 1024 * 1024;
/// Telegram's documented maximum sum of width and height for photos.
const TELEGRAM_PHOTO_MAX_DIMENSION_SUM: u64 = 10_000;
/// Telegram's documented maximum width/height ratio for photos.
const TELEGRAM_PHOTO_MAX_ASPECT_RATIO: f64 = 20.0;
/// Telegram's documented photo caption limit after entity parsing.
const TELEGRAM_PHOTO_CAPTION_LIMIT_CHARS: usize = 1024;
/// Telegram's documented rich message text limit.
const TELEGRAM_RICH_MESSAGE_LIMIT_CHARS: usize = 32_768;

/// Telegram update subset handled by this bot.
#[derive(Clone, Debug, Deserialize)]
pub struct Update {
    /// Monotonic Telegram update id used to suppress webhook retries.
    pub update_id: Option<i64>,
    /// Incoming message.
    pub message: Option<Message>,
    /// Callback query from inline keyboard.
    pub callback_query: Option<CallbackQuery>,
    /// Inline query used as `@bot query`.
    pub inline_query: Option<InlineQuery>,
}

/// Telegram message subset used by the app.
#[derive(Clone, Debug, Deserialize)]
pub struct Message {
    /// Telegram message id, present for editable callback messages.
    pub message_id: Option<i64>,
    /// Chat object.
    pub chat: Chat,
    /// Sender.
    pub from: Option<User>,
    /// Message text.
    pub text: Option<String>,
}

/// Telegram chat subset.
#[derive(Clone, Debug, Deserialize)]
pub struct Chat {
    /// Chat id.
    pub id: i64,
}

/// Telegram user subset.
#[derive(Clone, Debug, Deserialize)]
pub struct User {
    /// User id.
    pub id: i64,
}

/// Telegram callback query subset.
#[derive(Clone, Debug, Deserialize)]
pub struct CallbackQuery {
    /// Callback query id.
    pub id: String,
    /// Sender.
    pub from: User,
    /// Message that owns the button.
    pub message: Option<Message>,
    /// Callback data.
    pub data: Option<String>,
}

/// Telegram inline query subset.
#[derive(Clone, Debug, Deserialize)]
pub struct InlineQuery {
    /// Inline query id.
    pub id: String,
    /// Sender.
    pub from: User,
    /// Query text.
    pub query: String,
    /// Offset requested by Telegram for inline result pagination.
    #[serde(default)]
    pub offset: String,
    /// User location when BotFather inline location data is enabled.
    pub location: Option<Location>,
}

/// Telegram location subset included in inline queries.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Location {
    /// Latitude in degrees.
    pub latitude: f64,
    /// Longitude in degrees.
    pub longitude: f64,
    /// Optional horizontal accuracy radius in meters.
    pub horizontal_accuracy: Option<f64>,
}

/// Telegram Bot API client.
#[derive(Clone)]
pub struct TelegramClient {
    client: Client,
    token: String,
}

impl TelegramClient {
    /// Creates a Telegram API client.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            token: token.into(),
        }
    }

    /// Sends a "typing" chat action while the user waits.
    pub async fn send_typing(&self, chat_id: i64) -> Result<()> {
        self.post_json(
            "sendChatAction",
            &serde_json::json!({"chat_id": chat_id, "action": "typing"}),
        )
        .await?;
        Ok(())
    }

    /// Sends an HTML-formatted text message.
    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> Result<()> {
        let mut payload = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true
        });
        if let Some(markup) = reply_markup {
            payload["reply_markup"] = serde_json::to_value(markup)?;
        }
        self.post_json("sendMessage", &payload).await?;
        Ok(())
    }

    /// Edits an HTML-formatted text message in place.
    pub async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> Result<()> {
        let mut payload = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true
        });
        if let Some(markup) = reply_markup {
            payload["reply_markup"] = serde_json::to_value(markup)?;
        }
        self.post_json("editMessageText", &payload).await?;
        Ok(())
    }

    /// Sends file result buttons, adding in-place pagination when enabled.
    pub async fn send_file_buttons(
        &self,
        chat_id: i64,
        files: &[FileHit],
        preferences: &Preferences,
    ) -> Result<()> {
        if files.is_empty() {
            self.send_message(chat_id, "No files found.", None).await?;
            return Ok(());
        }
        let total_pages = page_count(files.len());
        let token = if preferences.pagination_enabled && files.len() > BUTTON_PAGE_SIZE {
            Some(store_file_list(files).await)
        } else {
            None
        };
        let buttons = file_buttons_page(files, preferences, token.as_deref(), 0, total_pages);
        self.send_message(
            chat_id,
            &paginated_title("Files", 0, total_pages),
            Some(InlineKeyboardMarkup {
                inline_keyboard: buttons,
            }),
        )
        .await
    }

    /// Sends category result buttons.
    pub async fn send_category_buttons(
        &self,
        chat_id: i64,
        categories: &[CategoryHit],
        pagination_enabled: bool,
    ) -> Result<()> {
        if categories.is_empty() {
            self.send_message(chat_id, "No categories found.", None)
                .await?;
            return Ok(());
        }
        self.send_category_buttons_with_title(chat_id, "Categories", categories, pagination_enabled)
            .await
    }

    /// Sends category result buttons with a custom heading.
    async fn send_category_buttons_with_title(
        &self,
        chat_id: i64,
        title: &str,
        categories: &[CategoryHit],
        pagination_enabled: bool,
    ) -> Result<()> {
        let total_pages = page_count(categories.len());
        let token = if pagination_enabled && categories.len() > BUTTON_PAGE_SIZE {
            Some(store_category_list(categories).await)
        } else {
            None
        };
        let page_kind = if title == "Subcategories" { "s" } else { "c" };
        let rows = category_buttons_page(categories, token.as_deref(), 0, total_pages, page_kind);
        self.send_message(
            chat_id,
            &paginated_title(title, 0, total_pages),
            Some(InlineKeyboardMarkup {
                inline_keyboard: rows,
            }),
        )
        .await
    }

    /// Sends subcategory result buttons after a category was opened.
    pub async fn send_subcategory_buttons(
        &self,
        chat_id: i64,
        categories: &[CategoryHit],
        pagination_enabled: bool,
    ) -> Result<()> {
        if categories.is_empty() {
            self.send_message(chat_id, "No subcategories found.", None)
                .await?;
            return Ok(());
        }
        self.send_category_buttons_with_title(
            chat_id,
            "Subcategories",
            categories,
            pagination_enabled,
        )
        .await
    }

    /// Sends a compact list of plain file links.
    pub async fn send_plain_files(&self, chat_id: i64, files: &[FileHit]) -> Result<()> {
        let text = if files.is_empty() {
            "No files found.".to_string()
        } else {
            files
                .iter()
                .take(10)
                .filter_map(file_link)
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.send_message(chat_id, &text, None).await
    }

    /// Sends image previews using the configured Telegram preview layout.
    pub async fn send_image_previews(
        &self,
        chat_id: i64,
        files: &[FileHit],
        max_images: usize,
        preferences: &Preferences,
        _send_overflow_metadata: bool,
    ) -> Result<()> {
        let image_files = files
            .iter()
            .filter(|file| telegram_preview_url(file).is_some())
            .take(max_images)
            .collect::<Vec<_>>();
        if image_files.is_empty() {
            self.send_plain_files(chat_id, files).await?;
            return Ok(());
        }

        if preferences.rich_image_previews {
            match self
                .send_rich_image_previews(chat_id, &image_files, preferences)
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) => tracing::warn!(
                    error = %format!("{error:#}"),
                    "sendRichMessage failed; falling back to individual photos"
                ),
            }
        }

        self.send_individual_photo_previews(chat_id, &image_files, preferences)
            .await
    }

    /// Sends one Telegram rich message with image blocks and metadata captions.
    async fn send_rich_image_previews(
        &self,
        chat_id: i64,
        files: &[&FileHit],
        preferences: &Preferences,
    ) -> Result<()> {
        let html = rich_image_preview_html(files, preferences);
        ensure!(!html.is_empty(), "no rich image previews to send");
        self.post_json(
            "sendRichMessage",
            &serde_json::json!({
                "chat_id": chat_id,
                "rich_message": {
                    "html": html,
                    "skip_entity_detection": true
                }
            }),
        )
        .await?;
        Ok(())
    }

    /// Sends previews as individual photos so each caption stays attached to its image.
    async fn send_individual_photo_previews(
        &self,
        chat_id: i64,
        files: &[&FileHit],
        preferences: &Preferences,
    ) -> Result<()> {
        for file in files {
            if let Err(error) = self
                .send_single_photo_preview(chat_id, file, preferences)
                .await
            {
                tracing::warn!(
                    file = %file.file_name,
                    error = %format!("{error:#}"),
                    "sendPhoto failed; falling back to a plain Commons link"
                );
                if let Some(link) = file_link(file) {
                    self.send_message(chat_id, &link, None).await?;
                }
            }
        }
        Ok(())
    }

    /// Sends one image preview as a Telegram photo, trying safer URLs if needed.
    async fn send_single_photo_preview(
        &self,
        chat_id: i64,
        file: &FileHit,
        preferences: &Preferences,
    ) -> Result<()> {
        let caption =
            telegram_photo_caption(file, preferences).context("file has no Commons URL")?;
        let mut last_error = None;
        for media_url in telegram_preview_urls(file) {
            match self
                .post_json(
                    "sendPhoto",
                    &serde_json::json!({
                        "chat_id": chat_id,
                        "photo": media_url,
                        "caption": caption,
                        "parse_mode": "HTML"
                    }),
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        match last_error {
            Some(error) => Err(error),
            None => bail!("file has no Telegram-compatible image preview URL"),
        }
    }

    /// Answers a callback query to stop Telegram's progress indicator.
    pub async fn answer_callback_query(&self, callback_query_id: &str) -> Result<()> {
        self.post_json(
            "answerCallbackQuery",
            &serde_json::json!({"callback_query_id": callback_query_id}),
        )
        .await?;
        Ok(())
    }

    /// Sends the original Commons file as a Telegram document.
    pub async fn send_original_file(
        &self,
        chat_id: i64,
        file: &FileHit,
        commons: &CommonsClient,
        caption: &str,
    ) -> Result<()> {
        let url = file.url.as_deref().context("file has no original URL")?;
        if file.size_bytes <= TELEGRAM_REMOTE_URL_LIMIT_BYTES {
            self.post_json(
                "sendDocument",
                &serde_json::json!({
                    "chat_id": chat_id,
                    "document": url,
                    "caption": caption,
                    "parse_mode": "HTML"
                }),
            )
            .await?;
            return Ok(());
        }

        let bytes = commons.download_file(file).await?;
        let form = multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .text("caption", caption.to_string())
            .text("parse_mode", "HTML")
            .part(
                "document",
                multipart::Part::bytes(bytes.to_vec()).file_name(file.file_name.clone()),
            );
        self.client
            .post(self.method_url("sendDocument"))
            .multipart(form)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Answers an inline query with photo/article results.
    pub async fn answer_inline_query(
        &self,
        query_id: &str,
        files: &[FileHit],
        is_personal: bool,
        next_offset: Option<&str>,
    ) -> Result<()> {
        let results = files
            .iter()
            .take(50)
            .filter_map(inline_result)
            .collect::<Vec<_>>();
        let cache_time = if results.is_empty() { 1 } else { 60 };
        let mut payload = serde_json::json!({
            "inline_query_id": query_id,
            "results": results,
            "cache_time": cache_time,
            "is_personal": is_personal
        });
        if let Some(next_offset) = next_offset {
            payload["next_offset"] = serde_json::Value::String(next_offset.to_string());
        }
        self.post_json("answerInlineQuery", &payload).await?;
        Ok(())
    }

    /// Sends a JSON payload to a Telegram method.
    async fn post_json(
        &self,
        method: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let response = self
            .client
            .post(self.method_url(method))
            .json(payload)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            bail!("Telegram method {method} failed with HTTP {status}: {body}");
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// Builds the Telegram method URL.
    fn method_url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{method}", self.token)
    }
}

/// Selects the safest image URL for a `-img` media group item.
fn telegram_preview_url(file: &FileHit) -> Option<&str> {
    telegram_preview_urls(file).into_iter().next()
}

/// Builds the photo caption, shortening only when Telegram's caption limit requires it.
fn telegram_photo_caption(file: &FileHit, preferences: &Preferences) -> Option<String> {
    if !preferences.show_preview_metadata {
        return file_link(file);
    }
    let caption = format_file_metadata(file, preferences);
    if caption_exceeds_photo_limit(&caption) {
        Some(fit_file_metadata_photo_caption(file, preferences))
    } else {
        Some(caption)
    }
}

/// Fits metadata into a Telegram photo caption before using stronger compaction.
fn fit_file_metadata_photo_caption(file: &FileHit, preferences: &Preferences) -> String {
    let mut lines = format_file_metadata_lines(file, preferences);
    if let Some(caption) = drop_low_priority_metadata_lines_until_fit(&mut lines) {
        return caption;
    }
    compact_file_metadata_caption(file, preferences)
}

/// Returns true when a caption should not be sent as a Telegram photo caption.
fn caption_exceeds_photo_limit(caption: &str) -> bool {
    caption.chars().count() > TELEGRAM_PHOTO_CAPTION_LIMIT_CHARS
}

/// Builds rich HTML with image blocks and metadata text below every image.
fn rich_image_preview_html(files: &[&FileHit], preferences: &Preferences) -> String {
    let mut html = String::new();
    for file in files {
        let Some(media_url) = telegram_preview_url(file) else {
            continue;
        };
        let caption = if preferences.show_preview_metadata {
            format_file_metadata(file, preferences)
        } else {
            file_link(file).unwrap_or_else(|| escape_text(&file.file_name))
        };
        let mut figure = rich_image_figure_html(media_url, &caption);
        if rich_message_exceeds_limit(&figure) {
            figure = rich_image_figure_html(
                media_url,
                &compact_file_metadata_caption(file, preferences),
            );
        }
        let separator = if html.is_empty() { "" } else { "\n" };
        let candidate = format!("{html}{separator}{figure}");
        if rich_message_exceeds_limit(&candidate) {
            break;
        }
        html = candidate;
    }
    html
}

/// Formats one Telegram rich-message figure block.
fn rich_image_figure_html(media_url: &str, caption: &str) -> String {
    format!(
        "<figure><img src=\"{}\"/><figcaption>{}</figcaption></figure>",
        escape_attr(media_url),
        caption.replace('\n', "<br>")
    )
}

/// Returns true when rich message source should not be sent to Telegram.
fn rich_message_exceeds_limit(html: &str) -> bool {
    html.chars().count() > TELEGRAM_RICH_MESSAGE_LIMIT_CHARS
}

/// Lists Telegram-safe preview URLs from safest rendered thumbnail to original fallback.
fn telegram_preview_urls(file: &FileHit) -> Vec<&str> {
    if !file
        .mime
        .as_deref()
        .is_some_and(|mime| mime.starts_with("image/"))
    {
        return Vec::new();
    }
    let mut urls = Vec::new();
    if let Some(url) = file.thumb_url.as_deref() {
        urls.push(url);
    }
    if original_image_fits_telegram_photo_url(file)
        && let Some(url) = file.url.as_deref()
        && !urls.contains(&url)
    {
        urls.push(url);
    }
    urls
}

/// Returns true when the original Commons image should work as a Telegram photo URL.
fn original_image_fits_telegram_photo_url(file: &FileHit) -> bool {
    file.url
        .as_deref()
        .is_some_and(|url| url.starts_with("https://"))
        && file.size_bytes > 0
        && file.size_bytes <= TELEGRAM_PHOTO_URL_LIMIT_BYTES
        && !file.animated
        && telegram_photo_original_mime_supported(file.mime.as_deref())
        && dimensions_fit_telegram_photo_limits(file.width, file.height)
}

/// Returns true for original image MIME types Telegram reliably accepts as photos.
fn telegram_photo_original_mime_supported(mime: Option<&str>) -> bool {
    matches!(mime, Some("image/jpeg"))
}

/// Checks Telegram's documented photo dimensions and aspect-ratio limits.
fn dimensions_fit_telegram_photo_limits(width: Option<u64>, height: Option<u64>) -> bool {
    let (Some(width), Some(height)) = (width, height) else {
        return false;
    };
    if width == 0 || height == 0 {
        return false;
    }
    if width.saturating_add(height) > TELEGRAM_PHOTO_MAX_DIMENSION_SUM {
        return false;
    }
    let max_side = width.max(height) as f64;
    let min_side = width.min(height) as f64;
    max_side / min_side <= TELEGRAM_PHOTO_MAX_ASPECT_RATIO
}

/// Telegram inline keyboard markup.
#[derive(Clone, Debug, Serialize)]
pub struct InlineKeyboardMarkup {
    /// Button rows.
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

/// Telegram inline keyboard button.
#[derive(Clone, Debug, Serialize)]
pub struct InlineKeyboardButton {
    /// Button text.
    pub text: String,
    /// Callback data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
    /// External URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Telegram button style, where supported by clients.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
}

/// Creates file callback buttons.
pub fn file_buttons(
    files: &[FileHit],
    preferences: &Preferences,
) -> Vec<Vec<InlineKeyboardButton>> {
    file_buttons_page(files, preferences, None, 0, 1)
}

/// Creates one page of file callback buttons and optional pagination controls.
pub fn file_buttons_page(
    files: &[FileHit],
    preferences: &Preferences,
    page_token: Option<&str>,
    page_index: usize,
    total_pages: usize,
) -> Vec<Vec<InlineKeyboardButton>> {
    let mut rows = files
        .iter()
        .take(BUTTON_PAGE_SIZE)
        .map(|file| {
            vec![InlineKeyboardButton {
                text: file_button_label(file, preferences),
                callback_data: Some(format!("file:{}", file.page_id)),
                url: None,
                style: file_button_style(file),
            }]
        })
        .collect::<Vec<_>>();
    if let Some(token) = page_token
        && total_pages > 1
    {
        rows.push(pagination_row("f", token, page_index, total_pages));
    }
    rows
}

/// Creates one page of category callback buttons and optional pagination controls.
pub fn category_buttons_page(
    categories: &[CategoryHit],
    page_token: Option<&str>,
    page_index: usize,
    total_pages: usize,
    page_kind: &str,
) -> Vec<Vec<InlineKeyboardButton>> {
    let mut rows = categories
        .iter()
        .take(BUTTON_PAGE_SIZE)
        .map(|category| {
            vec![InlineKeyboardButton {
                text: category_button_label(category),
                callback_data: Some(format!("cat:{}", category.page_id)),
                url: None,
                style: None,
            }]
        })
        .collect::<Vec<_>>();
    if let Some(token) = page_token
        && total_pages > 1
    {
        rows.push(pagination_row(page_kind, token, page_index, total_pages));
    }
    rows
}

/// Formats a Telegram result heading with a one-based page marker.
pub fn paginated_title(title: &str, page_index: usize, total_pages: usize) -> String {
    if total_pages > 1 {
        format!("{title} {}/{}", page_index + 1, total_pages)
    } else {
        title.to_string()
    }
}

/// Builds the navigation row used by in-place pagination callbacks.
fn pagination_row(
    kind: &str,
    page_token: &str,
    page_index: usize,
    total_pages: usize,
) -> Vec<InlineKeyboardButton> {
    let mut row = Vec::new();
    if page_index > 0 {
        row.push(InlineKeyboardButton {
            text: "Prev".into(),
            callback_data: Some(format!("pg:{kind}:{page_token}:{}", page_index - 1)),
            url: None,
            style: None,
        });
    }
    if page_index + 1 < total_pages {
        row.push(InlineKeyboardButton {
            text: "Next".into(),
            callback_data: Some(format!("pg:{kind}:{page_token}:{}", page_index + 1)),
            url: None,
            style: None,
        });
    }
    row
}

/// Formats file metadata as an HTML caption/message.
pub fn format_file_metadata(file: &FileHit, preferences: &Preferences) -> String {
    format_file_metadata_lines(file, preferences).join("\n")
}

/// Builds file metadata lines before they are joined for Telegram.
fn format_file_metadata_lines(file: &FileHit, preferences: &Preferences) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(url) = &file.description_url {
        lines.push(format!(
            "<b><a href=\"{}\">{}</a></b>",
            escape_attr(url),
            escape_text(&file.file_name)
        ));
    } else {
        lines.push(format!("<b>{}</b>", escape_text(&file.file_name)));
    }
    if let Some(license) = &file.license_short_name {
        if let Some(url) = &file.license_url {
            lines.push(format!(
                "License: <a href=\"{}\">{}</a>",
                escape_attr(url),
                escape_text(license)
            ));
        } else {
            lines.push(format!("License: {}", escape_text(license)));
        }
    }
    if let Some(caption) = file
        .caption_text
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        lines.push(format!("Caption: {}", escape_text(caption)));
    }
    if let Some(description) = file
        .description_text
        .as_deref()
        .filter(|value| !value.is_empty())
        .filter(|description| {
            !file
                .caption_text
                .as_deref()
                .is_some_and(|caption| same_display_text(caption, description))
        })
    {
        lines.push(format!("Description: {}", escape_text(description)));
    }
    if let Some(uploader) = &file.uploader {
        lines.push(format!(
            "Uploader: <a href=\"https://commons.wikimedia.org/wiki/User:{}\">{}</a>",
            urlencoding::encode(uploader),
            escape_text(uploader)
        ));
    }
    if let Some(artist) = &file.artist
        && !same_uploader_and_author(file.uploader.as_deref(), artist)
    {
        lines.push(format!("Author: {}", escape_text(artist)));
    }
    if let Some(date) = &file.timestamp {
        lines.push(format!("Uploaded: {}", escape_text(date)));
    }
    if let Some(date) = &file.date_text {
        lines.push(format!("Date: {}", escape_text(date)));
    }
    if let (Some(width), Some(height)) = (file.width, file.height)
        && file_has_visual_dimensions(file, width, height)
    {
        lines.push(format!("Dimensions: {} x {}", width, height));
    }
    if let Some(model) = file
        .camera_model
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        lines.push(camera_model_line(model));
    }
    lines.extend(exif_detail_lines(file, false));
    if file.animated {
        lines.push("Animated: yes".to_string());
    }
    if let Some(duration) = file.duration_seconds {
        lines.push(format!("Duration: {:.1} seconds", duration));
    }
    if let Some(coords) = file.coordinates {
        lines.push(format!(
            "Geolocation: <a href=\"{}\">{:.6}, {:.6}</a>",
            coords.openstreetmap_url(),
            coords.lat,
            coords.lon
        ));
    }
    if let Some(count) = file.version_count
        && count > 1
    {
        lines.push(format!("{count} versions were uploaded"));
    }
    if preferences.show_sha1
        && let Some(sha1) = &file.sha1
    {
        lines.push(format!("SHA-1: <code>{}</code>", escape_text(sha1)));
    }
    if let Some(url) = file.history_url() {
        lines.push(format!("<a href=\"{}\">History</a>", escape_attr(&url)));
    }
    lines
}

/// Formats preview metadata compactly enough for Telegram photo captions.
fn compact_file_metadata_caption(file: &FileHit, preferences: &Preferences) -> String {
    let mut lines = vec![compact_title_line(file)];
    if let Some(license) = &file.license_short_name {
        if let Some(url) = &file.license_url {
            lines.push(format!(
                "<a href=\"{}\">{}</a>",
                escape_attr(url),
                escape_text(&truncate_visible_text(license, 40))
            ));
        } else {
            lines.push(format!(
                "License: {}",
                escape_text(&truncate_visible_text(license, 40))
            ));
        }
    }
    if let Some(caption) = file
        .caption_text
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        lines.push(format!(
            "Caption: {}",
            escape_text(&truncate_visible_text(caption, 180))
        ));
    }
    if let Some(description) = file
        .description_text
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .filter(|description| {
            !file
                .caption_text
                .as_deref()
                .is_some_and(|caption| same_display_text(caption, description))
        })
    {
        lines.push(format!(
            "Description: {}",
            escape_text(&truncate_visible_text(description, 220))
        ));
    }
    if let Some(uploader) = &file.uploader {
        lines.push(format!(
            "Uploader: <a href=\"https://commons.wikimedia.org/wiki/User:{}\">{}</a>",
            urlencoding::encode(uploader),
            escape_text(&truncate_visible_text(uploader, 60))
        ));
    }
    if let Some(artist) = &file.artist
        && !same_uploader_and_author(file.uploader.as_deref(), artist)
    {
        lines.push(format!(
            "Author: {}",
            escape_text(&truncate_visible_text(artist, 90))
        ));
    }
    if let Some(date) = &file.timestamp {
        lines.push(format!("Uploaded: {}", escape_text(date)));
    }
    if let Some(date) = &file.date_text {
        lines.push(format!(
            "Date: {}",
            escape_text(&truncate_visible_text(date, 60))
        ));
    }
    if let (Some(width), Some(height)) = (file.width, file.height)
        && file_has_visual_dimensions(file, width, height)
    {
        lines.push(format!("Dimensions: {} x {}", width, height));
    }
    if let Some(model) = file
        .camera_model
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        lines.push(compact_camera_model_line(model));
    }
    lines.extend(exif_detail_lines(file, true));
    if file.animated {
        lines.push("Animated: yes".to_string());
    }
    if let Some(coords) = file.coordinates {
        lines.push(format!(
            "Geolocation: <a href=\"{}\">{:.6}, {:.6}</a>",
            coords.openstreetmap_url(),
            coords.lat,
            coords.lon
        ));
    }
    if let Some(count) = file.version_count
        && count > 1
    {
        lines.push(format!("{count} versions were uploaded"));
    }
    if preferences.show_sha1
        && let Some(sha1) = &file.sha1
    {
        lines.push(format!("SHA-1: <code>{}</code>", escape_text(sha1)));
    }

    compact_caption_fit(lines, file)
}

/// Builds a short linked file-title line for compact captions.
fn compact_title_line(file: &FileHit) -> String {
    let name = truncate_visible_text(&file.file_name, 120);
    if let Some(url) = &file.description_url {
        format!(
            "<b><a href=\"{}\">{}</a></b>",
            escape_attr(url),
            escape_text(&name)
        )
    } else {
        format!("<b>{}</b>", escape_text(&name))
    }
}

/// Drops low-priority lines until the compact caption fits Telegram.
fn compact_caption_fit(mut lines: Vec<String>, file: &FileHit) -> String {
    if let Some(caption) = drop_low_priority_metadata_lines_until_fit(&mut lines) {
        return caption;
    }
    while lines.len() > 1 {
        let caption = lines.join("\n");
        if !caption_exceeds_photo_limit(&caption) {
            return caption;
        }
        lines.pop();
    }
    let caption = lines.join("\n");
    if !caption_exceeds_photo_limit(&caption) {
        return caption;
    }
    file_link(file).unwrap_or_else(|| escape_text(&truncate_visible_text(&file.file_name, 120)))
}

/// Drops user-approved low-priority metadata rows in a stable order.
fn drop_low_priority_metadata_lines_until_fit(lines: &mut Vec<String>) -> Option<String> {
    let caption = lines.join("\n");
    if !caption_exceeds_photo_limit(&caption) {
        return Some(caption);
    }
    for matcher in LOW_PRIORITY_PHOTO_CAPTION_LINES {
        remove_first_caption_line_matching(lines, matcher);
        let caption = lines.join("\n");
        if !caption_exceeds_photo_limit(&caption) {
            return Some(caption);
        }
    }
    None
}

/// Removes the first caption row that matches the given predicate.
fn remove_first_caption_line_matching(lines: &mut Vec<String>, matcher: fn(&str) -> bool) {
    if let Some(index) = lines.iter().position(|line| matcher(line)) {
        lines.remove(index);
    }
}

/// Rows that are removed first when Telegram photo captions are too long.
const LOW_PRIORITY_PHOTO_CAPTION_LINES: [fn(&str) -> bool; 5] = [
    is_f_number_line,
    is_iso_speed_line,
    is_lens_focal_length_line,
    is_exposure_time_line,
    is_history_line,
];

/// Returns true for the F-number EXIF row.
fn is_f_number_line(line: &str) -> bool {
    line.contains(">F-number</a>:") || line.starts_with("F-number:")
}

/// Returns true for the ISO speed EXIF row.
fn is_iso_speed_line(line: &str) -> bool {
    line.contains(">ISO speed rating</a>:") || line.starts_with("ISO speed rating:")
}

/// Returns true for the focal length EXIF row.
fn is_lens_focal_length_line(line: &str) -> bool {
    line.contains(">Lens focal length</a>:") || line.starts_with("Lens focal length:")
}

/// Returns true for the exposure-time EXIF row.
fn is_exposure_time_line(line: &str) -> bool {
    line.contains(">Exposure time</a>:") || line.starts_with("Exposure time:")
}

/// Returns true for the file-history link row.
fn is_history_line(line: &str) -> bool {
    line.contains(">History</a>") || line == "History"
}

/// Collapses whitespace and truncates visible text without cutting HTML markup.
fn truncate_visible_text(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let keep = max_chars.saturating_sub(3);
    let mut shortened = compact.chars().take(keep).collect::<String>();
    shortened.push_str("...");
    shortened
}

/// Formats a clickable camera model metadata line.
fn camera_model_line(model: &str) -> String {
    linked_camera_model_line(model, model)
}

/// Formats a clickable camera model metadata line for compact captions.
fn compact_camera_model_line(model: &str) -> String {
    linked_camera_model_line(model, &truncate_visible_text(model, 60))
}

/// Links camera models like the Commons EXIF table does.
fn linked_camera_model_line(model: &str, display: &str) -> String {
    format!(
        "Camera model: <a href=\"{}\">{}</a>",
        escape_attr(&camera_model_url(model)),
        escape_text(display)
    )
}

/// Builds a stable search URL for a raw camera model string.
fn camera_model_url(model: &str) -> String {
    format!(
        "https://en.wikipedia.org/wiki/Special:Search?search={}",
        urlencoding::encode(model)
    )
}

/// Formats optional EXIF detail rows for metadata captions.
fn exif_detail_lines(file: &FileHit, compact: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(value) = metadata_value_for_caption(file.f_number.as_deref(), compact) {
        lines.push(exif_detail_line(
            "F-number",
            "https://en.wikipedia.org/wiki/F-number",
            &value,
        ));
    }
    if let Some(value) = metadata_value_for_caption(file.iso_speed.as_deref(), compact) {
        lines.push(exif_detail_line(
            "ISO speed rating",
            "https://en.wikipedia.org/wiki/Film_speed#Digital_camera_ISO_speed_and_exposure_index",
            &value,
        ));
    }
    if let Some(value) = metadata_value_for_caption(file.focal_length.as_deref(), compact) {
        lines.push(exif_detail_line(
            "Lens focal length",
            "https://en.wikipedia.org/wiki/Focal_length",
            &value,
        ));
    }
    if let Some(value) = metadata_value_for_caption(file.exposure_time.as_deref(), compact) {
        lines.push(exif_detail_line(
            "Exposure time",
            "https://en.wikipedia.org/wiki/Shutter_speed",
            &value,
        ));
    }
    lines
}

/// Formats one linked EXIF label and escaped value.
fn exif_detail_line(label: &str, url: &str, value: &str) -> String {
    format!(
        "<a href=\"{}\">{}</a>: {}",
        escape_attr(url),
        escape_text(label),
        escape_text(value)
    )
}

/// Returns a trimmed metadata value, shortening it only for compact captions.
fn metadata_value_for_caption(value: Option<&str>, compact: bool) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if compact {
        Some(truncate_visible_text(value, 40))
    } else {
        Some(value.to_string())
    }
}

/// Formats a category info message.
pub fn format_category_info(category: &CategoryInfo) -> String {
    let mut lines = Vec::new();
    lines.push(format!("<b>{}</b>", escape_text(&category.title)));
    if let Some(description) = &category.description {
        lines.push(escape_text(description));
    }
    if let Some(url) = category.wikidata_url() {
        lines.push(format!("<a href=\"{}\">Wikidata</a>", escape_attr(&url)));
    }
    if let Some(claims_html) = &category.wikidata_claims_html {
        lines.push(claims_html.clone());
    }
    lines.join("\n\n")
}

/// Chooses delivery mode for Telegram search results.
pub async fn send_search_results(
    telegram: &TelegramClient,
    chat_id: i64,
    files: &[FileHit],
    preferences: &Preferences,
    links_flag: bool,
    image_previews_flag: bool,
) -> Result<()> {
    if links_flag || preferences.delivery_mode == DeliveryMode::Links10 {
        telegram.send_plain_files(chat_id, files).await
    } else if image_previews_flag {
        telegram
            .send_image_previews(chat_id, files, 10, preferences, false)
            .await
    } else if preferences.delivery_mode == DeliveryMode::Images10 {
        telegram
            .send_image_previews(chat_id, files, 10, preferences, true)
            .await
    } else if preferences.delivery_mode == DeliveryMode::Images20 {
        telegram
            .send_image_previews(chat_id, files, 20, preferences, true)
            .await
    } else {
        telegram
            .send_file_buttons(chat_id, files, preferences)
            .await
    }
}

/// Formats a file button label.
fn file_button_label(file: &FileHit, preferences: &Preferences) -> String {
    let mut label = file.file_name.clone();
    if preferences.show_file_size {
        label.push_str(&format!(" ({})", human_bytes(file.size_bytes)));
    }
    if label.chars().count() > 64 {
        label = label.chars().take(61).collect::<String>();
        label.push_str("...");
    }
    label
}

/// Returns Telegram button style for special file cases.
fn file_button_style(file: &FileHit) -> Option<String> {
    if file.size_bytes > TELEGRAM_REMOTE_URL_LIMIT_BYTES {
        Some("danger".into())
    } else if file.is_audio() {
        Some("primary".into())
    } else {
        None
    }
}

/// Formats a category button label.
fn category_button_label(category: &CategoryHit) -> String {
    match category.file_count {
        Some(count) => format!("{} ({count})", category.display_title),
        None => category.display_title.clone(),
    }
}

/// Builds an inline query result for a Commons file.
fn inline_result(file: &FileHit) -> Option<serde_json::Value> {
    let id = file.page_id.to_string();
    let description = inline_description(file);
    if file
        .mime
        .as_deref()
        .is_some_and(|mime| mime.starts_with("image/"))
    {
        let photo_url = telegram_preview_url(file)?;
        return Some(serde_json::json!({
            "type": "photo",
            "id": id,
            "photo_url": photo_url,
            "thumbnail_url": file.thumb_url.as_deref().unwrap_or(photo_url),
            "title": file.file_name,
            "description": description,
            "caption": format!("<a href=\"{}\">{}</a>", escape_attr(file.description_url.as_ref()?), escape_text(&file.file_name)),
            "parse_mode": "HTML"
        }));
    }
    Some(serde_json::json!({
        "type": "article",
        "id": id,
        "title": file.file_name,
        "description": description,
        "input_message_content": {
            "message_text": format!("<a href=\"{}\">{}</a>", escape_attr(file.description_url.as_ref()?), escape_text(&file.file_name)),
            "parse_mode": "HTML"
        }
    }))
}

/// Returns concise inline result description text.
fn inline_description(file: &FileHit) -> String {
    file.caption_text
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            file.description_text
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
        .map(short_inline_text)
        .unwrap_or_else(|| {
            file.mime
                .clone()
                .unwrap_or_else(|| human_bytes(file.size_bytes))
        })
}

/// Shortens inline descriptions to a practical single-line preview.
fn short_inline_text(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > 120 {
        let mut shortened = compact.chars().take(117).collect::<String>();
        shortened.push_str("...");
        shortened
    } else {
        compact
    }
}

/// Returns true when two metadata strings would read the same to a user.
fn same_display_text(left: &str, right: &str) -> bool {
    normalized_display_text(left) == normalized_display_text(right)
}

/// Returns true when uploader and author metadata name the same person.
fn same_uploader_and_author(uploader: Option<&str>, author: &str) -> bool {
    uploader
        .is_some_and(|uploader| normalized_credit_text(uploader) == normalized_credit_text(author))
}

/// Normalizes visible credit text for uploader/author duplicate checks.
fn normalized_credit_text(value: &str) -> String {
    normalized_display_text(&value.replace('_', " "))
}

/// Normalizes visible metadata text for duplicate checks.
fn normalized_display_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Returns true when width and height are meaningful visual dimensions.
fn file_has_visual_dimensions(file: &FileHit, width: u64, height: u64) -> bool {
    if width == 0 || height == 0 {
        return false;
    }
    file.mime.as_deref().is_some_and(|mime| {
        mime.starts_with("image/")
            || mime.starts_with("video/")
            || mime == "application/pdf"
            || mime == "image/vnd.djvu"
    })
}

/// Escapes text for Telegram HTML parse mode.
pub fn escape_text(value: &str) -> String {
    html_escape::encode_text(value).to_string()
}

/// Escapes an attribute value for Telegram HTML parse mode.
fn escape_attr(value: &str) -> String {
    html_escape::encode_double_quoted_attribute(value).to_string()
}

/// Builds a compact HTML link to a Commons file page.
fn file_link(file: &FileHit) -> Option<String> {
    Some(format!(
        "<a href=\"{}\">{}</a>",
        escape_attr(file.description_url.as_ref()?),
        escape_text(&file.file_name)
    ))
}

/// Formats a byte count compactly.
pub fn human_bytes(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = units[0];
    for next in units.iter().skip(1) {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if unit == "B" {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Update, category_buttons_page, drop_low_priority_metadata_lines_until_fit, file_buttons,
        file_buttons_page, format_file_metadata, human_bytes, inline_result, paginated_title,
        rich_image_preview_html, short_inline_text, telegram_photo_caption, telegram_preview_url,
    };
    use crate::models::{CategoryHit, FileHit, Preferences};

    #[test]
    fn deserializes_inline_location() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 1,
                "inline_query": {
                    "id": "abc",
                    "from": {"id": 42},
                    "query": "Minsk",
                    "offset": "50",
                    "location": {
                        "latitude": 53.9023,
                        "longitude": 27.5619,
                        "horizontal_accuracy": 25.0
                    }
                }
            }"#,
        )
        .unwrap();

        let inline_query = update.inline_query.unwrap();
        let location = inline_query.location.unwrap();
        assert_eq!(update.update_id, Some(1));
        assert_eq!(inline_query.query, "Minsk");
        assert_eq!(inline_query.offset, "50");
        assert_eq!(location.latitude, 53.9023);
        assert_eq!(location.longitude, 27.5619);
        assert_eq!(location.horizontal_accuracy, Some(25.0));
    }

    #[test]
    fn styles_large_and_audio_buttons() {
        let large = FileHit {
            page_id: 1,
            file_name: "Large.tif".into(),
            size_bytes: 21 * 1024 * 1024,
            ..FileHit::default()
        };
        let audio = FileHit {
            page_id: 2,
            file_name: "Sound.flac".into(),
            mime: Some("audio/flac".into()),
            ..FileHit::default()
        };
        let buttons = file_buttons(&[large, audio], &Preferences::default());
        assert_eq!(buttons[0][0].style.as_deref(), Some("danger"));
        assert_eq!(buttons[1][0].style.as_deref(), Some("primary"));
    }

    #[test]
    fn renders_paginated_file_navigation() {
        let files = (0..20)
            .map(|page_id| FileHit {
                page_id,
                file_name: format!("{page_id}.jpg"),
                ..FileHit::default()
            })
            .collect::<Vec<_>>();

        let rows = file_buttons_page(&files, &Preferences::default(), Some("abcdef"), 1, 3);

        let nav = rows.last().unwrap();
        assert_eq!(paginated_title("Files", 1, 3), "Files 2/3");
        assert_eq!(nav[0].text, "Prev");
        assert_eq!(nav[0].callback_data.as_deref(), Some("pg:f:abcdef:0"));
        assert_eq!(nav[1].text, "Next");
        assert_eq!(nav[1].callback_data.as_deref(), Some("pg:f:abcdef:2"));
    }

    #[test]
    fn renders_subcategory_pagination_callbacks() {
        let categories = vec![CategoryHit {
            page_id: 42,
            title: "Category:Minsk".into(),
            display_title: "Minsk".into(),
            file_count: None,
        }];

        let rows = category_buttons_page(&categories, Some("abcdef"), 0, 2, "s");

        assert_eq!(rows[0][0].callback_data.as_deref(), Some("cat:42"));
        assert_eq!(
            rows.last().unwrap()[0].callback_data.as_deref(),
            Some("pg:s:abcdef:1")
        );
    }

    #[test]
    fn formats_metadata_with_links() {
        let prefs = Preferences {
            show_sha1: true,
            ..Preferences::default()
        };
        let file = FileHit {
            title: "File:A & B.jpg".into(),
            file_name: "A & B.jpg".into(),
            description_url: Some("https://commons.wikimedia.org/wiki/File:A".into()),
            uploader: Some("Example".into()),
            caption_text: Some("A caption".into()),
            description_text: Some("A longer description".into()),
            artist: Some("Jane Example".into()),
            timestamp: Some("2025-12-31T00:00:00Z".into()),
            camera_model: Some("Canon EOS 6D".into()),
            f_number: Some("f/7.1".into()),
            iso_speed: Some("1,250".into()),
            focal_length: Some("50 mm".into()),
            exposure_time: Some("1/500 sec (0.002)".into()),
            sha1: Some("abc".into()),
            ..FileHit::default()
        };
        let text = format_file_metadata(&file, &prefs);
        assert!(text.contains(
            "<b><a href=\"https://commons.wikimedia.org/wiki/File:A\">A &amp; B.jpg</a></b>"
        ));
        assert!(!text.contains("Original location"));
        assert!(!text.contains("Size:"));
        assert!(text.contains("Caption: A caption"));
        assert!(text.contains("Description: A longer description"));
        assert!(text.contains("User:Example"));
        assert!(text.find("Uploader:").unwrap() < text.find("Author:").unwrap());
        assert!(text.find("Author:").unwrap() < text.find("Uploaded:").unwrap());
        assert!(text.contains(
            "Camera model: <a href=\"https://en.wikipedia.org/wiki/Special:Search?search=Canon%20EOS%206D\">Canon EOS 6D</a>"
        ));
        assert!(
            text.contains("<a href=\"https://en.wikipedia.org/wiki/F-number\">F-number</a>: f/7.1")
        );
        assert!(text.contains(
            "<a href=\"https://en.wikipedia.org/wiki/Film_speed#Digital_camera_ISO_speed_and_exposure_index\">ISO speed rating</a>: 1,250"
        ));
        assert!(text.contains(
            "<a href=\"https://en.wikipedia.org/wiki/Focal_length\">Lens focal length</a>: 50 mm"
        ));
        assert!(text.contains(
            "<a href=\"https://en.wikipedia.org/wiki/Shutter_speed\">Exposure time</a>: 1/500 sec (0.002)"
        ));
        assert!(text.contains("SHA-1"));
        assert!(text.ends_with(
            "<a href=\"https://commons.wikimedia.org/w/index.php?title=File%3AA%20%26%20B.jpg&amp;action=history\">History</a>"
        ));
    }

    #[test]
    fn suppresses_duplicate_description_and_audio_dimensions() {
        let file = FileHit {
            file_name: "Sound.ogg".into(),
            mime: Some("audio/ogg".into()),
            width: Some(0),
            height: Some(0),
            caption_text: Some("Same text".into()),
            description_text: Some("Same text".into()),
            ..FileHit::default()
        };
        let text = format_file_metadata(&file, &Preferences::default());

        assert!(text.contains("Caption: Same text"));
        assert!(!text.contains("Description:"));
        assert!(!text.contains("Dimensions: 0 x 0"));
    }

    #[test]
    fn suppresses_author_when_same_as_uploader() {
        let file = FileHit {
            file_name: "A.jpg".into(),
            uploader: Some("Vitaly_Zdanevich".into()),
            artist: Some("Vitaly Zdanevich".into()),
            ..FileHit::default()
        };
        let text = format_file_metadata(&file, &Preferences::default());

        assert!(text.contains("Uploader:"));
        assert!(!text.contains("Author:"));
    }

    #[test]
    fn formats_category_info_with_wikidata_claims() {
        let category = crate::models::CategoryInfo {
            title: "Category:Minsk".into(),
            wikidata_item: Some("Q2280".into()),
            wikidata_claims_html: Some(
                "<a href=\"https://www.wikidata.org/wiki/Property:P31\">instance of (P31)</a>: <a href=\"https://www.wikidata.org/wiki/Q515\">city (Q515)</a>"
                    .into(),
            ),
            ..Default::default()
        };
        let text = super::format_category_info(&category);

        assert!(text.contains("<a href=\"https://www.wikidata.org/wiki/Q2280\">Wikidata</a>"));
        assert!(text.contains("instance of (P31)"));
        assert!(text.contains("city (Q515)"));
    }

    #[test]
    fn formats_human_bytes() {
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn preview_caption_uses_metadata_by_default() {
        let file = FileHit {
            file_name: "A.jpg".into(),
            description_url: Some("https://commons.wikimedia.org/wiki/File:A.jpg".into()),
            uploader: Some("Example".into()),
            artist: Some("Jane Example".into()),
            timestamp: Some("2025-12-31T00:00:00Z".into()),
            ..FileHit::default()
        };
        let caption = telegram_photo_caption(&file, &Preferences::default()).unwrap();

        assert!(caption.contains("Uploader:"));
        assert!(caption.contains("Author:"));
        assert!(caption.contains("Uploaded:"));
    }

    #[test]
    fn preview_caption_can_disable_metadata() {
        let preferences = Preferences {
            show_preview_metadata: false,
            ..Preferences::default()
        };
        let file = FileHit {
            file_name: "A.jpg".into(),
            description_url: Some("https://commons.wikimedia.org/wiki/File:A.jpg".into()),
            uploader: Some("Example".into()),
            ..FileHit::default()
        };
        let caption = telegram_photo_caption(&file, &preferences).unwrap();

        assert_eq!(
            caption,
            "<a href=\"https://commons.wikimedia.org/wiki/File:A.jpg\">A.jpg</a>"
        );
        assert!(!caption.contains("Uploader:"));
    }

    #[test]
    fn preview_caption_shortens_when_metadata_exceeds_photo_caption_limit() {
        let file = FileHit {
            file_name: "A.jpg".into(),
            description_url: Some("https://commons.wikimedia.org/wiki/File:A.jpg".into()),
            caption_text: Some("Useful caption".into()),
            description_text: Some("Very long description ".repeat(80)),
            uploader: Some("Example".into()),
            artist: Some("Very long author ".repeat(100)),
            license_short_name: Some("CC BY-SA 4.0".into()),
            license_url: Some("https://creativecommons.org/licenses/by-sa/4.0".into()),
            ..FileHit::default()
        };
        let caption = telegram_photo_caption(&file, &Preferences::default()).unwrap();

        assert!(caption.chars().count() <= super::TELEGRAM_PHOTO_CAPTION_LIMIT_CHARS);
        assert!(caption.contains(
            "<a href=\"https://creativecommons.org/licenses/by-sa/4.0\">CC BY-SA 4.0</a>"
        ));
        assert!(!caption.contains("License:"));
        assert!(caption.contains("Caption: Useful caption"));
        assert!(caption.contains("Description: Very long description"));
        assert!(caption.contains("Uploader:"));
        assert!(!caption.contains("History"));
        assert_ne!(
            caption,
            "<a href=\"https://commons.wikimedia.org/wiki/File:A.jpg\">A.jpg</a>"
        );
    }

    #[test]
    fn photo_caption_pruning_drops_only_first_needed_low_priority_line() {
        let mut lines = caption_lines_needing_removed_indexes(&[1]);

        let caption = drop_low_priority_metadata_lines_until_fit(&mut lines).unwrap();

        assert!(!caption.contains("F-number"));
        assert!(caption.contains("ISO speed rating"));
        assert!(caption.contains("Lens focal length"));
        assert!(caption.contains("Exposure time"));
        assert!(caption.contains("History"));
        assert!(caption.chars().count() <= super::TELEGRAM_PHOTO_CAPTION_LIMIT_CHARS);
    }

    #[test]
    fn photo_caption_pruning_continues_until_history_when_needed() {
        let mut lines = caption_lines_needing_removed_indexes(&[1, 2, 3, 4, 5]);

        let caption = drop_low_priority_metadata_lines_until_fit(&mut lines).unwrap();

        assert!(!caption.contains("F-number"));
        assert!(!caption.contains("ISO speed rating"));
        assert!(!caption.contains("Lens focal length"));
        assert!(!caption.contains("Exposure time"));
        assert!(!caption.contains("History"));
        assert!(caption.chars().count() <= super::TELEGRAM_PHOTO_CAPTION_LIMIT_CHARS);
    }

    #[test]
    fn rich_preview_html_keeps_metadata_with_each_image() {
        let file = FileHit {
            file_name: "A.jpg".into(),
            description_url: Some("https://commons.wikimedia.org/wiki/File:A.jpg".into()),
            thumb_url: Some("https://upload.wikimedia.org/thumb/a.jpg".into()),
            mime: Some("image/jpeg".into()),
            caption_text: Some("Useful caption".into()),
            uploader: Some("Example".into()),
            ..FileHit::default()
        };

        let html = rich_image_preview_html(&[&file], &Preferences::default());

        assert!(html.contains("<figure><img src=\"https://upload.wikimedia.org/thumb/a.jpg\"/>"));
        assert!(html.contains("<figcaption>"));
        assert!(html.contains("Caption: Useful caption"));
        assert!(html.contains("Uploader:"));
        assert!(html.contains("<br>"));
    }

    #[test]
    fn rich_preview_html_respects_telegram_limit() {
        let file = FileHit {
            file_name: "A.jpg".into(),
            description_url: Some("https://commons.wikimedia.org/wiki/File:A.jpg".into()),
            thumb_url: Some("https://upload.wikimedia.org/thumb/a.jpg".into()),
            mime: Some("image/jpeg".into()),
            description_text: Some("Long description ".repeat(10_000)),
            ..FileHit::default()
        };

        let html = rich_image_preview_html(&[&file], &Preferences::default());

        assert!(html.chars().count() <= super::TELEGRAM_RICH_MESSAGE_LIMIT_CHARS);
        assert!(html.contains("A.jpg"));
    }

    #[test]
    fn rich_preview_html_can_disable_metadata() {
        let preferences = Preferences {
            show_preview_metadata: false,
            rich_image_previews: true,
            ..Preferences::default()
        };
        let file = FileHit {
            file_name: "A.jpg".into(),
            description_url: Some("https://commons.wikimedia.org/wiki/File:A.jpg".into()),
            thumb_url: Some("https://upload.wikimedia.org/thumb/a.jpg".into()),
            mime: Some("image/jpeg".into()),
            uploader: Some("Example".into()),
            ..FileHit::default()
        };

        let html = rich_image_preview_html(&[&file], &preferences);

        assert!(
            html.contains("<a href=\"https://commons.wikimedia.org/wiki/File:A.jpg\">A.jpg</a>")
        );
        assert!(!html.contains("Uploader:"));
    }

    fn caption_lines_needing_removed_indexes(removed_indexes: &[usize]) -> Vec<String> {
        let mut lines = base_priority_caption_lines();
        let base_after_removal = lines
            .iter()
            .enumerate()
            .filter(|(index, _)| !removed_indexes.contains(index))
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let filler =
            super::TELEGRAM_PHOTO_CAPTION_LIMIT_CHARS - base_after_removal.chars().count() - 1;
        lines.push("x".repeat(filler));
        lines
    }

    fn base_priority_caption_lines() -> Vec<String> {
        vec![
            "<b>A.jpg</b>".into(),
            "F-number: f/7".into(),
            "ISO speed rating: 400".into(),
            "Lens focal length: 11 mm".into(),
            "Exposure time: 10/850 sec".into(),
            "History".into(),
        ]
    }

    #[test]
    fn preview_prefers_thumbnail_for_small_supported_photo() {
        let file = FileHit {
            file_name: "Small.jpg".into(),
            mime: Some("image/jpeg".into()),
            url: Some("https://upload.wikimedia.org/original.jpg".into()),
            thumb_url: Some("https://upload.wikimedia.org/thumb.jpg".into()),
            size_bytes: 4 * 1024 * 1024,
            width: Some(3000),
            height: Some(2000),
            ..FileHit::default()
        };

        assert_eq!(
            telegram_preview_url(&file),
            Some("https://upload.wikimedia.org/thumb.jpg")
        );
    }

    #[test]
    fn inline_photo_uses_preview_url() {
        let file = FileHit {
            page_id: 7,
            file_name: "Small.jpg".into(),
            mime: Some("image/jpeg".into()),
            url: Some("https://upload.wikimedia.org/original.jpg".into()),
            thumb_url: Some("https://upload.wikimedia.org/thumb.jpg".into()),
            description_url: Some("https://commons.wikimedia.org/wiki/File:Small.jpg".into()),
            size_bytes: 4 * 1024 * 1024,
            width: Some(3000),
            height: Some(2000),
            ..FileHit::default()
        };

        let result = inline_result(&file).unwrap();

        assert_eq!(
            result["photo_url"],
            "https://upload.wikimedia.org/thumb.jpg"
        );
        assert_eq!(
            result["thumbnail_url"],
            "https://upload.wikimedia.org/thumb.jpg"
        );
    }

    #[test]
    fn inline_result_uses_metadata_description() {
        let file = FileHit {
            page_id: 7,
            file_name: "Small.jpg".into(),
            mime: Some("image/jpeg".into()),
            thumb_url: Some("https://upload.wikimedia.org/thumb.jpg".into()),
            description_url: Some("https://commons.wikimedia.org/wiki/File:Small.jpg".into()),
            caption_text: Some("Short caption".into()),
            ..FileHit::default()
        };

        let result = inline_result(&file).unwrap();

        assert_eq!(result["description"], "Short caption");
    }

    #[test]
    fn inline_description_is_shortened() {
        assert_eq!(short_inline_text(&"word ".repeat(40)).chars().count(), 120);
    }

    #[test]
    fn preview_falls_back_to_thumbnail_for_large_original() {
        let file = FileHit {
            file_name: "Large.jpg".into(),
            mime: Some("image/jpeg".into()),
            url: Some("https://upload.wikimedia.org/original.jpg".into()),
            thumb_url: Some("https://upload.wikimedia.org/thumb.jpg".into()),
            size_bytes: 6 * 1024 * 1024,
            width: Some(3000),
            height: Some(2000),
            ..FileHit::default()
        };

        assert_eq!(
            telegram_preview_url(&file),
            Some("https://upload.wikimedia.org/thumb.jpg")
        );
    }

    #[test]
    fn preview_uses_original_when_thumbnail_is_missing() {
        let file = FileHit {
            file_name: "Small.jpg".into(),
            mime: Some("image/jpeg".into()),
            url: Some("https://upload.wikimedia.org/original.jpg".into()),
            size_bytes: 500 * 1024,
            width: Some(1200),
            height: Some(900),
            ..FileHit::default()
        };

        assert_eq!(
            telegram_preview_url(&file),
            Some("https://upload.wikimedia.org/original.jpg")
        );
    }

    #[test]
    fn preview_falls_back_to_thumbnail_for_non_jpeg_original() {
        let file = FileHit {
            file_name: "Screenshot.png".into(),
            mime: Some("image/png".into()),
            url: Some("https://upload.wikimedia.org/original.png".into()),
            thumb_url: Some("https://upload.wikimedia.org/thumb.png".into()),
            size_bytes: 500 * 1024,
            width: Some(1200),
            height: Some(900),
            ..FileHit::default()
        };

        assert_eq!(
            telegram_preview_url(&file),
            Some("https://upload.wikimedia.org/thumb.png")
        );
    }

    #[test]
    fn preview_falls_back_to_thumbnail_for_unsupported_or_extreme_originals() {
        let svg = FileHit {
            file_name: "Vector.svg".into(),
            mime: Some("image/svg+xml".into()),
            url: Some("https://upload.wikimedia.org/original.svg".into()),
            thumb_url: Some("https://upload.wikimedia.org/thumb.png".into()),
            size_bytes: 200 * 1024,
            width: Some(2000),
            height: Some(1000),
            ..FileHit::default()
        };
        let panorama = FileHit {
            file_name: "Panorama.jpg".into(),
            mime: Some("image/jpeg".into()),
            url: Some("https://upload.wikimedia.org/original.jpg".into()),
            thumb_url: Some("https://upload.wikimedia.org/thumb.jpg".into()),
            size_bytes: 500 * 1024,
            width: Some(21_000),
            height: Some(1000),
            ..FileHit::default()
        };

        assert_eq!(
            telegram_preview_url(&svg),
            Some("https://upload.wikimedia.org/thumb.png")
        );
        assert_eq!(
            telegram_preview_url(&panorama),
            Some("https://upload.wikimedia.org/thumb.jpg")
        );
    }
}
