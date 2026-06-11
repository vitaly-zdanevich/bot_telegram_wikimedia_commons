use serde::{Deserialize, Serialize};
use std::fmt;

/// Telegram's maximum bot upload size requested for this project, in bytes.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Files above this size should be uploaded multipart instead of sent by URL.
pub const TELEGRAM_REMOTE_URL_LIMIT_BYTES: u64 = 20 * 1024 * 1024;

/// Image extensions currently accepted by Wikimedia Commons.
pub const COMMONS_IMAGE_EXTENSIONS: &[&str] = &[
    "gif", "jpeg", "jpg", "png", "svg", "tif", "tiff", "webp", "xcf",
];

/// Audio extensions currently accepted by Wikimedia Commons.
pub const COMMONS_AUDIO_EXTENSIONS: &[&str] =
    &["flac", "mid", "midi", "mp3", "oga", "ogg", "opus", "wav"];

/// Video extensions currently accepted by Wikimedia Commons.
pub const COMMONS_VIDEO_EXTENSIONS: &[&str] = &["mpeg", "mpg", "ogv", "webm"];

/// Document extensions currently accepted by Wikimedia Commons.
pub const COMMONS_DOCUMENT_EXTENSIONS: &[&str] = &["djvu", "pdf"];

/// 3D/model extensions currently accepted by Wikimedia Commons.
pub const COMMONS_MODEL_EXTENSIONS: &[&str] = &["stl"];

/// All file extensions currently accepted by Wikimedia Commons.
pub const COMMONS_SUPPORTED_EXTENSIONS: &[&str] = &[
    "djvu", "flac", "gif", "jpeg", "jpg", "mid", "midi", "mp3", "mpeg", "mpg", "oga", "ogg", "ogv",
    "opus", "pdf", "png", "stl", "svg", "tif", "tiff", "wav", "webm", "webp", "xcf",
];

/// A high-level file-family filter.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum FileType {
    /// Return any supported Commons media type.
    #[default]
    All,
    /// Return image-like files.
    Images,
    /// Return audio files.
    Audio,
    /// Return video files.
    Video,
}

impl FileType {
    /// Parses a user-facing preference or CLI value.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" => Some(Self::All),
            "image" | "images" => Some(Self::Images),
            "audio" | "sound" | "music" => Some(Self::Audio),
            "video" | "videos" => Some(Self::Video),
            _ => None,
        }
    }

    /// Returns the CirrusSearch file type predicate, if one is needed.
    pub fn cirrus_filetype(&self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Images => Some("bitmap"),
            Self::Audio => Some("audio"),
            Self::Video => Some("video"),
        }
    }

    /// Returns the stable value stored in preferences.
    pub fn as_pref_value(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Images => "images",
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }
}

/// How Telegram search results should be delivered.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum DeliveryMode {
    /// File buttons with metadata on click.
    #[default]
    Buttons,
    /// Ten compact Commons links in one message.
    Links10,
    /// Ten image previews in one media group.
    Images10,
    /// Twenty image previews in two media groups.
    Images20,
}

impl DeliveryMode {
    /// Parses a stable preference value.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "buttons" => Some(Self::Buttons),
            "links" | "links10" => Some(Self::Links10),
            "img" | "img10" => Some(Self::Images10),
            "images10" => Some(Self::Images10),
            "images20" => Some(Self::Images20),
            _ => None,
        }
    }

    /// Returns the stable value stored in preferences.
    pub fn as_pref_value(&self) -> &'static str {
        match self {
            Self::Buttons => "buttons",
            Self::Links10 => "links10",
            Self::Images10 => "images10",
            Self::Images20 => "images20",
        }
    }
}

/// PDF/DjVu delivery behavior.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum DocumentPageMode {
    /// Send the original document.
    #[default]
    Original,
    /// Send rendered page previews from Wikimedia.
    RenderedPages,
}

impl DocumentPageMode {
    /// Parses a stable preference value.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "original" => Some(Self::Original),
            "rendered" | "pages" => Some(Self::RenderedPages),
            _ => None,
        }
    }

    /// Returns the stable value stored in preferences.
    pub fn as_pref_value(&self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::RenderedPages => "rendered",
        }
    }
}

/// Telegram user preferences persisted in DynamoDB when enabled.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Preferences {
    /// Whether category buttons should include direct file counts.
    pub show_category_counts: bool,
    /// Preferred result delivery mode.
    pub delivery_mode: DeliveryMode,
    /// Preferred file-family filter.
    pub file_type: FileType,
    /// Optional file extension filter without a leading dot.
    pub extension: Option<String>,
    /// Favorite categories shown by `/help`.
    pub favorite_categories: Vec<String>,
    /// Categories hidden from search results.
    pub blacklist_categories: Vec<String>,
    /// Uploaders hidden from search results.
    pub blacklist_uploaders: Vec<String>,
    /// Whether metadata responses include SHA-1.
    pub show_sha1: bool,
    /// Whether button labels include file size.
    pub show_file_size: bool,
    /// Whether `-img` preview captions include file metadata.
    #[serde(default = "default_true")]
    pub show_preview_metadata: bool,
    /// PDF delivery mode.
    pub pdf_mode: DocumentPageMode,
    /// DjVu delivery mode.
    pub djvu_mode: DocumentPageMode,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            show_category_counts: false,
            delivery_mode: DeliveryMode::Buttons,
            file_type: FileType::All,
            extension: None,
            favorite_categories: Vec::new(),
            blacklist_categories: Vec::new(),
            blacklist_uploaders: Vec::new(),
            show_sha1: false,
            show_file_size: false,
            show_preview_metadata: true,
            pdf_mode: DocumentPageMode::Original,
            djvu_mode: DocumentPageMode::Original,
        }
    }
}

/// Returns true for serde defaults that should be enabled for old preference JSON.
fn default_true() -> bool {
    true
}

/// A parsed top-level user intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Intent {
    /// Show help text.
    Help,
    /// Show or edit preferences.
    Preferences,
    /// Show operational statistics.
    Stats,
    /// Search for files.
    FileSearch(SearchQuery),
    /// Search for category pages.
    CategorySearch(String),
    /// Empty or unsupported input.
    Empty,
}

/// A parsed Commons file search.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SearchQuery {
    /// Original user input.
    pub raw: String,
    /// Remaining free-text terms.
    pub terms: Vec<String>,
    /// File-family override from syntax.
    pub file_type: Option<FileType>,
    /// Optional file extension without dot.
    pub extension: Option<String>,
    /// Optional category name without `Category:`.
    pub category: Option<String>,
    /// Optional uploader username.
    pub user: Option<String>,
    /// Optional date filter.
    pub date: Option<DateFilter>,
    /// File-size predicates.
    pub size_filters: Vec<SizeFilter>,
    /// Compact `-links` delivery.
    pub links_flag: bool,
    /// Telegram image-preview delivery.
    pub image_previews_flag: bool,
    /// Sort final results by size.
    pub sort_by_size: bool,
    /// CLI-only bypass for Telegram's 50 MB filter.
    pub bypass_telegram_limit: bool,
}

impl SearchQuery {
    /// Returns the free-text terms joined for Commons search.
    pub fn term_text(&self) -> String {
        self.terms.join(" ").trim().to_string()
    }
}

/// A supported upload-date filter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DateFilter {
    /// Specific UTC year.
    Year(i32),
    /// Specific UTC date in `YYYY-MM-DD`.
    Day(String),
    /// Previous N days.
    RelativeDays(u32),
    /// Previous month.
    PreviousMonth,
    /// Previous year.
    PreviousYear,
}

impl fmt::Display for DateFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Year(year) => write!(f, "{year}"),
            Self::Day(day) => f.write_str(day),
            Self::RelativeDays(days) => write!(f, "{days}days"),
            Self::PreviousMonth => f.write_str("month"),
            Self::PreviousYear => f.write_str("year"),
        }
    }
}

/// A file-size predicate in bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SizeFilter {
    /// Predicate operator.
    pub op: SizeOp,
    /// Predicate threshold in bytes.
    pub bytes: u64,
}

/// Supported size predicate operators.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SizeOp {
    /// Strictly greater than the threshold.
    GreaterThan,
    /// Strictly less than the threshold.
    LessThan,
}

/// A Commons file returned by search or category enumeration.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileHit {
    /// MediaWiki page id.
    pub page_id: u64,
    /// Full title, usually `File:...`.
    pub title: String,
    /// File name without namespace.
    pub file_name: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Width in pixels.
    pub width: Option<u64>,
    /// Height in pixels.
    pub height: Option<u64>,
    /// MIME type.
    pub mime: Option<String>,
    /// MediaWiki media type.
    pub media_type: Option<String>,
    /// Original file URL.
    pub url: Option<String>,
    /// Commons description page URL.
    pub description_url: Option<String>,
    /// Thumbnail URL.
    pub thumb_url: Option<String>,
    /// Latest uploader.
    pub uploader: Option<String>,
    /// Latest upload timestamp.
    pub timestamp: Option<String>,
    /// SHA-1 hash.
    pub sha1: Option<String>,
    /// Number of upload versions returned by the API.
    pub version_count: Option<usize>,
    /// Short license name.
    pub license_short_name: Option<String>,
    /// License URL.
    pub license_url: Option<String>,
    /// Commons caption/object name metadata.
    pub caption_text: Option<String>,
    /// Commons description metadata.
    pub description_text: Option<String>,
    /// Author/artist metadata.
    pub artist: Option<String>,
    /// Date metadata.
    pub date_text: Option<String>,
    /// Camera model from EXIF metadata.
    pub camera_model: Option<String>,
    /// Exposure time from EXIF metadata, formatted for display.
    pub exposure_time: Option<String>,
    /// F-number from EXIF metadata, formatted for display.
    pub f_number: Option<String>,
    /// ISO speed rating from EXIF metadata, formatted for display.
    pub iso_speed: Option<String>,
    /// Lens focal length from EXIF metadata, formatted for display.
    pub focal_length: Option<String>,
    /// Coordinates, if present.
    pub coordinates: Option<Coordinates>,
    /// Whether Commons metadata marks the file as animated.
    pub animated: bool,
    /// Media duration in seconds.
    pub duration_seconds: Option<f64>,
}

impl FileHit {
    /// Returns the lower-case extension from the file name.
    pub fn extension(&self) -> Option<String> {
        self.file_name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
    }

    /// Returns true when Commons or MIME metadata indicates audio.
    pub fn is_audio(&self) -> bool {
        self.media_type
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("AUDIO"))
            || self
                .mime
                .as_deref()
                .is_some_and(|value| value.starts_with("audio/"))
    }

    /// Returns true for PDF and DjVu files.
    pub fn is_paginated_document(&self) -> bool {
        matches!(self.extension().as_deref(), Some("pdf" | "djvu" | "djv"))
    }

    /// Returns the Commons file-history action URL.
    pub fn history_url(&self) -> Option<String> {
        let title = self.history_title()?;
        Some(format!(
            "https://commons.wikimedia.org/w/index.php?title={}&action=history",
            urlencoding::encode(&title)
        ))
    }

    /// Returns the best MediaWiki title for history links.
    fn history_title(&self) -> Option<String> {
        if !self.title.trim().is_empty() {
            return Some(self.title.clone());
        }
        if !self.file_name.trim().is_empty() {
            return Some(format!("File:{}", self.file_name));
        }
        None
    }
}

/// A latitude/longitude pair.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Coordinates {
    /// Latitude.
    pub lat: f64,
    /// Longitude.
    pub lon: f64,
}

impl Coordinates {
    /// Returns a clickable OpenStreetMap URL.
    pub fn openstreetmap_url(&self) -> String {
        format!(
            "https://www.openstreetmap.org/?mlat={:.6}&mlon={:.6}#map=15/{:.6}/{:.6}",
            self.lat, self.lon, self.lat, self.lon
        )
    }
}

/// A Commons category search result.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CategoryHit {
    /// MediaWiki page id.
    pub page_id: u64,
    /// Full title, usually `Category:...`.
    pub title: String,
    /// Title without namespace.
    pub display_title: String,
    /// Optional direct file count.
    pub file_count: Option<u64>,
}

/// Category page summary and child listings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CategoryInfo {
    /// Category page id.
    pub page_id: u64,
    /// Full category title.
    pub title: String,
    /// Plain-text description.
    pub description: Option<String>,
    /// Wikidata item id, if linked.
    pub wikidata_item: Option<String>,
    /// Rendered clickable Wikidata key/value claims.
    pub wikidata_claims_html: Option<String>,
    /// Direct files.
    pub files: Vec<FileHit>,
    /// Direct subcategories.
    pub subcategories: Vec<CategoryHit>,
}

impl CategoryInfo {
    /// Returns a Wikidata URL when the category is linked to Wikidata.
    pub fn wikidata_url(&self) -> Option<String> {
        self.wikidata_item
            .as_ref()
            .map(|id| format!("https://www.wikidata.org/wiki/{id}"))
    }
}
