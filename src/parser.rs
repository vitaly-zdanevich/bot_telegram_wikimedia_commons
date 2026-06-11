use crate::models::{
    COMMONS_AUDIO_EXTENSIONS, COMMONS_DOCUMENT_EXTENSIONS, COMMONS_IMAGE_EXTENSIONS,
    COMMONS_MODEL_EXTENSIONS, COMMONS_SUPPORTED_EXTENSIONS, COMMONS_VIDEO_EXTENSIONS, DateFilter,
    FileType, Intent, SearchQuery, SizeFilter, SizeOp,
};
use once_cell::sync::Lazy;
use regex::Regex;

static TOKEN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#""([^"]+)"|'([^']+)'|(\S+)"#).expect("valid token regex"));
static DATE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(date|datum|d):\s*(\d{4}-\d{2}-\d{2}|\d{4}|\d+days|month|year|monat|jahr)$")
        .expect("valid date regex")
});
static USER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^(user|u|benutzer):\s*(.+)$").expect("valid user regex"));
static CATEGORY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(c|category|категория|катэгорыя|kategorie|kat|к|с):\s*(.+)$")
        .expect("valid category regex")
});
static SIZE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(size|s|groesse|größe|g):\s*([<>])\s*(\d+(?:\.\d+)?)\s*([kmgt]?b?)$")
        .expect("valid size regex")
});

/// Parses a Telegram or CLI command into a structured intent.
pub fn parse_intent(input: &str) -> Intent {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Intent::Empty;
    }

    if matches!(trimmed, "/start" | "/help" | "help") {
        return Intent::Help;
    }
    if matches!(
        trimmed,
        "/settings" | "/preferences" | "/prefs" | "settings" | "prefs" | "preferences"
    ) {
        return Intent::Preferences;
    }
    if matches!(trimmed, "/stat" | "/stats" | "stat" | "stats") {
        return Intent::Stats;
    }

    let tokens = tokenize(trimmed);
    if tokens.is_empty() {
        return Intent::Empty;
    }

    if let Some(category_query) = parse_category_search(&tokens) {
        return Intent::CategorySearch(category_query);
    }

    Intent::FileSearch(parse_search(trimmed, &tokens))
}

/// Parses a raw input string as a file-search query.
pub fn parse_search(raw: &str, tokens: &[String]) -> SearchQuery {
    let mut query = SearchQuery {
        raw: raw.to_string(),
        ..SearchQuery::default()
    };
    let mut index = 0;

    while index < tokens.len() {
        let token = &tokens[index];
        let lower = normalize_alias(token);

        if is_links_results_flag(&lower) {
            query.links_flag = true;
            index += 1;
            continue;
        }
        if is_image_preview_flag(&lower) {
            query.image_previews_flag = true;
            if query.file_type.is_none() {
                query.file_type = Some(FileType::Images);
            }
            index += 1;
            continue;
        }
        if lower == "--sort-size" || lower == "--sort-by-size" || lower == "sort:size" {
            query.sort_by_size = true;
            index += 1;
            continue;
        }
        if lower == "--bypass-telegram-limit" {
            query.bypass_telegram_limit = true;
            index += 1;
            continue;
        }

        if let Some(file_type) = parse_file_type_prefix(&lower) {
            query.file_type = Some(file_type);
            if token.contains(':') {
                let suffix = token
                    .split_once(':')
                    .map(|(_, suffix)| suffix.trim())
                    .unwrap_or("");
                if !suffix.is_empty() {
                    query.terms.push(suffix.to_string());
                }
            } else if index + 1 < tokens.len() {
                // `audio something` is a prefix, not a search term itself.
            }
            index += 1;
            continue;
        }

        if let Some(extension) = parse_known_extension(&lower) {
            query.extension = Some(extension);
            index += 1;
            continue;
        }

        if let Some(captures) = DATE_RE.captures(token) {
            if let Some(date) = parse_date_filter(&captures[2]) {
                query.date = Some(date);
            }
            index += 1;
            continue;
        }

        if (lower == "date" || lower == "datum" || lower == "d")
            && let Some(value) = tokens.get(index + 1)
        {
            query.date = parse_date_filter(value);
            index += 2;
            continue;
        }

        if let Some(captures) = SIZE_RE.captures(token) {
            if let Some(size) = parse_size_filter(&captures[2], &captures[3], &captures[4]) {
                query.size_filters.push(size);
            }
            index += 1;
            continue;
        }

        if let Some(captures) = USER_RE.captures(token) {
            let value = captures[2].trim();
            if value.is_empty() && index + 1 < tokens.len() {
                query.user = Some(tokens[index + 1].clone());
                index += 2;
            } else {
                query.user = Some(value.to_string());
                index += 1;
            }
            continue;
        }

        if (lower == "user" || lower == "u" || lower == "benutzer")
            && let Some(value) = tokens.get(index + 1)
        {
            query.user = Some(value.clone());
            index += 2;
            continue;
        }

        if let Some(captures) = CATEGORY_RE.captures(token) {
            let value = captures[2].trim();
            if !value.is_empty() {
                query.category = Some(normalize_category(value));
                index += 1;
                continue;
            }
        }

        if is_category_alias(lower.trim_end_matches(':'))
            && let Some(value) = tokens.get(index + 1)
        {
            query.category = Some(normalize_category(value));
            index += 2;
            continue;
        }

        query.terms.push(token.clone());
        index += 1;
    }

    query
}

/// Returns true for the compact link-result flag used in Telegram chats.
fn is_links_results_flag(value: &str) -> bool {
    matches!(value, "-links" | "–links" | "—links")
}

/// Returns true for the image-preview flag used in Telegram chats.
fn is_image_preview_flag(value: &str) -> bool {
    matches!(value, "-img" | "–img" | "—img")
}

/// Splits a command line into shell-like tokens with simple quote support.
pub fn tokenize(input: &str) -> Vec<String> {
    TOKEN_RE
        .captures_iter(input)
        .filter_map(|captures| {
            captures
                .get(1)
                .or_else(|| captures.get(2))
                .or_else(|| captures.get(3))
                .map(|m| m.as_str().trim().to_string())
        })
        .filter(|token| !token.is_empty())
        .collect()
}

/// Normalizes a user-provided category name.
pub fn normalize_category(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("Category:")
        .trim_start_matches("category:")
        .replace('_', " ")
}

/// Returns true when the extension should be treated as an image.
pub fn is_image_extension(ext: &str) -> bool {
    let ext = ext.to_ascii_lowercase();
    COMMONS_IMAGE_EXTENSIONS.contains(&ext.as_str()) || matches!(ext.as_str(), "avif" | "bmp")
}

/// Returns true when the extension should be treated as audio.
pub fn is_audio_extension(ext: &str) -> bool {
    let ext = ext.to_ascii_lowercase();
    COMMONS_AUDIO_EXTENSIONS.contains(&ext.as_str())
}

/// Returns true when the extension should be treated as video.
pub fn is_video_extension(ext: &str) -> bool {
    let ext = ext.to_ascii_lowercase();
    COMMONS_VIDEO_EXTENSIONS.contains(&ext.as_str()) || ext == "mp4"
}

/// Returns true when Commons accepts the extension for upload/search filtering.
pub fn is_commons_supported_extension(ext: &str) -> bool {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    COMMONS_SUPPORTED_EXTENSIONS.contains(&ext.as_str())
}

/// Parses a date filter value.
fn parse_date_filter(value: &str) -> Option<DateFilter> {
    let lower = value.trim().to_ascii_lowercase();
    if lower == "month" || lower == "monat" {
        return Some(DateFilter::PreviousMonth);
    }
    if lower == "year" || lower == "jahr" {
        return Some(DateFilter::PreviousYear);
    }
    if let Some(days) = lower.strip_suffix("days") {
        return days.parse::<u32>().ok().map(DateFilter::RelativeDays);
    }
    if lower.len() == 4 {
        return lower.parse::<i32>().ok().map(DateFilter::Year);
    }
    if lower.len() == 10
        && lower.as_bytes().get(4) == Some(&b'-')
        && lower.as_bytes().get(7) == Some(&b'-')
    {
        return Some(DateFilter::Day(lower));
    }
    None
}

/// Parses a file-size predicate.
fn parse_size_filter(op: &str, number: &str, unit: &str) -> Option<SizeFilter> {
    let number = number.parse::<f64>().ok()?;
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let op = match op {
        ">" => SizeOp::GreaterThan,
        "<" => SizeOp::LessThan,
        _ => return None,
    };
    Some(SizeFilter {
        op,
        bytes: (number * multiplier) as u64,
    })
}

/// Detects a top-level category search prefix.
fn parse_category_search(tokens: &[String]) -> Option<String> {
    let first = normalize_alias(&tokens[0]);
    if let Some(captures) = CATEGORY_RE.captures(&tokens[0]) {
        let prefix = normalize_alias(&captures[1]);
        if is_category_word(&prefix) {
            let query = captures[2].trim();
            if !query.is_empty() {
                return Some(normalize_category(query));
            }
        }
    }
    if is_category_word(first.trim_end_matches(':')) && tokens.len() > 1 {
        return Some(tokens[1..].join(" "));
    }
    None
}

/// Returns true for aliases that mean "category search".
fn is_category_word(value: &str) -> bool {
    matches!(
        value,
        "c" | "category"
            | "kategorie"
            | "kat"
            | "k"
            | "категория"
            | "катэгорыя"
            | "к"
            | "с"
    )
}

/// Returns true for aliases that mean "category filter".
fn is_category_alias(value: &str) -> bool {
    matches!(
        value,
        "c" | "category"
            | "kategorie"
            | "kat"
            | "k"
            | "категория"
            | "катэгорыя"
            | "к"
            | "с"
    )
}

/// Parses image/audio/video query prefixes.
fn parse_file_type_prefix(value: &str) -> Option<FileType> {
    let normalized = value
        .split_once(':')
        .map(|(prefix, _)| prefix)
        .unwrap_or(value)
        .trim_end_matches(':');
    match normalized {
        "audio" | "sound" | "music" | "ton" | "klang" | "musik" | "музыка" | "звук" | "аудио"
        | "аудыё" => Some(FileType::Audio),
        "image" | "images" | "img" | "i" | "bild" | "bilder" | "foto" | "fotos" | "выява"
        | "ваява" | "в" => Some(FileType::Images),
        "video" | "videos" => Some(FileType::Video),
        _ => None,
    }
}

/// Parses a known file extension filter.
fn parse_known_extension(value: &str) -> Option<String> {
    let ext = value.trim_start_matches('.').to_ascii_lowercase();
    if is_image_extension(&ext)
        || is_audio_extension(&ext)
        || is_video_extension(&ext)
        || COMMONS_DOCUMENT_EXTENSIONS.contains(&ext.as_str())
        || COMMONS_MODEL_EXTENSIONS.contains(&ext.as_str())
    {
        Some(ext)
    } else {
        None
    }
}

/// Lowercases aliases while preserving non-ASCII letters.
fn normalize_alias(value: &str) -> String {
    value.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DateFilter, SizeOp};

    #[test]
    fn parses_audio_user_date_and_size() {
        let tokens = tokenize("audio:something user:Vitaly_Zdanevich date:7days s:>10MB s:<20MB");
        let query = parse_search("raw", &tokens);
        assert_eq!(query.file_type, Some(FileType::Audio));
        assert_eq!(query.terms, vec!["something"]);
        assert_eq!(query.user, Some("Vitaly_Zdanevich".into()));
        assert_eq!(query.date, Some(DateFilter::RelativeDays(7)));
        assert_eq!(query.size_filters.len(), 2);
        assert_eq!(query.size_filters[0].op, SizeOp::GreaterThan);
        assert_eq!(query.size_filters[0].bytes, 10 * 1024 * 1024);
        assert_eq!(query.size_filters[1].op, SizeOp::LessThan);
    }

    #[test]
    fn parses_category_intent_aliases() {
        assert_eq!(
            parse_intent("Катэгорыя нешта"),
            Intent::CategorySearch("нешта".into())
        );
        assert_eq!(
            parse_intent("category: Minsk"),
            Intent::CategorySearch("Minsk".into())
        );
        assert_eq!(
            parse_intent("c minsk"),
            Intent::CategorySearch("minsk".into())
        );
        assert_eq!(
            parse_intent("c:minsk"),
            Intent::CategorySearch("minsk".into())
        );
        match parse_intent("cat minsk") {
            Intent::FileSearch(query) => assert_eq!(query.terms, vec!["cat", "minsk"]),
            other => panic!("expected FileSearch for literal cat query, got {other:?}"),
        }
    }

    #[test]
    fn parses_category_filter_with_extension() {
        let tokens = tokenize("flac minsk c some_category");
        let query = parse_search("raw", &tokens);
        assert_eq!(query.extension, Some("flac".into()));
        assert_eq!(query.terms, vec!["minsk"]);
        assert_eq!(query.category, Some("some category".into()));
    }

    #[test]
    fn parses_settings_intent_alias() {
        assert_eq!(parse_intent("/settings"), Intent::Preferences);
        assert_eq!(parse_intent("settings"), Intent::Preferences);
    }

    #[test]
    fn parses_result_delivery_flags() {
        let tokens = tokenize("-img Minsk");
        let query = parse_search("raw", &tokens);
        assert!(query.image_previews_flag);
        assert_eq!(query.file_type, Some(FileType::Images));
        assert_eq!(query.terms, vec!["Minsk"]);

        let tokens = tokenize("—links Minsk");
        let query = parse_search("raw", &tokens);
        assert!(query.links_flag);
    }

    #[test]
    fn parses_belarusian_image_alias() {
        let tokens = tokenize("ваява:something user:Vitaly_Zdanevich d:month c:Minsk");
        let query = parse_search("raw", &tokens);
        assert_eq!(query.file_type, Some(FileType::Images));
        assert_eq!(query.terms, vec!["something"]);
        assert_eq!(query.date, Some(DateFilter::PreviousMonth));
        assert_eq!(query.category, Some("Minsk".into()));
    }

    #[test]
    fn parses_german_aliases() {
        assert_eq!(
            parse_intent("Kategorie Berlin"),
            Intent::CategorySearch("Berlin".into())
        );
        let tokens = tokenize("bild:Berlin benutzer:Example datum:monat groesse:<1MB");
        let query = parse_search("raw", &tokens);
        assert_eq!(query.file_type, Some(FileType::Images));
        assert_eq!(query.user, Some("Example".into()));
        assert_eq!(query.date, Some(DateFilter::PreviousMonth));
        assert_eq!(query.size_filters[0].op, SizeOp::LessThan);
    }

    #[test]
    fn parses_short_user_alias() {
        let tokens = tokenize("u:Красный -img");
        let query = parse_search("raw", &tokens);
        assert_eq!(query.user, Some("Красный".into()));
        assert!(query.image_previews_flag);
        assert!(query.terms.is_empty());

        let tokens = tokenize("u Красный");
        let query = parse_search("raw", &tokens);
        assert_eq!(query.user, Some("Красный".into()));
        assert!(query.terms.is_empty());

        let tokens = tokenize("U:Красный");
        let query = parse_search("raw", &tokens);
        assert_eq!(query.user, Some("Красный".into()));
        assert!(query.terms.is_empty());
    }

    #[test]
    fn tokenizes_quoted_terms() {
        assert_eq!(
            tokenize(r#"jpg "New York" user:Some_User"#),
            vec!["jpg", "New York", "user:Some_User"]
        );
    }
}
