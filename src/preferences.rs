use crate::aws::AwsJsonClient;
use crate::config::Config;
use crate::models::{
    DEFAULT_INLINE_RESULT_COUNT, DeliveryMode, DocumentPageMode, FileType, Preferences,
    normalize_inline_result_count,
};
use anyhow::Result;
use once_cell::sync::Lazy;
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::sync::RwLock;

static RAM_CACHE: Lazy<RwLock<HashMap<i64, Preferences>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Preference storage backed by DynamoDB when stateful mode is enabled.
#[derive(Clone)]
pub struct PreferenceStore {
    table_name: Option<String>,
    stateless_mode: bool,
    aws: AwsJsonClient,
}

impl PreferenceStore {
    /// Creates a preference store from runtime configuration.
    pub fn new(config: &Config) -> Self {
        Self {
            table_name: config.dynamodb_table.clone(),
            stateless_mode: config.stateless_mode,
            aws: AwsJsonClient::new(config.aws_region.clone()),
        }
    }

    /// Returns preferences for a Telegram user, using RAM cache first.
    pub async fn get(&self, telegram_user_id: i64) -> Preferences {
        if self.stateless_mode || self.table_name.is_none() || !self.aws.has_credentials() {
            return Preferences::default();
        }
        if let Some(cached) = RAM_CACHE.read().await.get(&telegram_user_id).cloned() {
            return cached;
        }
        let loaded = self
            .load_from_dynamodb(telegram_user_id)
            .await
            .unwrap_or_default();
        RAM_CACHE
            .write()
            .await
            .insert(telegram_user_id, loaded.clone());
        loaded
    }

    /// Saves preferences for a Telegram user and refreshes the RAM cache.
    pub async fn put(&self, telegram_user_id: i64, preferences: &Preferences) -> Result<()> {
        RAM_CACHE
            .write()
            .await
            .insert(telegram_user_id, preferences.clone());
        if self.stateless_mode || self.table_name.is_none() || !self.aws.has_credentials() {
            return Ok(());
        }
        self.save_to_dynamodb(telegram_user_id, preferences).await
    }

    /// Loads one user preference document from DynamoDB.
    async fn load_from_dynamodb(&self, telegram_user_id: i64) -> Result<Preferences> {
        let table = self.table_name.as_ref().expect("checked by caller");
        let response = self
            .aws
            .post_json(
                "dynamodb",
                "DynamoDB_20120810.GetItem",
                json!({
                    "TableName": table,
                    "Key": {
                        "pk": {"S": format!("USER#{telegram_user_id}")},
                        "sk": {"S": "PREFERENCES"}
                    }
                }),
            )
            .await?;
        let Some(item) = response.get("Item") else {
            return Ok(Preferences::default());
        };
        Ok(item_to_preferences(item))
    }

    /// Writes one user preference document to DynamoDB.
    async fn save_to_dynamodb(
        &self,
        telegram_user_id: i64,
        preferences: &Preferences,
    ) -> Result<()> {
        let table = self.table_name.as_ref().expect("checked by caller");
        self.aws
            .post_json(
                "dynamodb",
                "DynamoDB_20120810.PutItem",
                json!({
                    "TableName": table,
                    "Item": preferences_to_item(telegram_user_id, preferences)
                }),
            )
            .await?;
        Ok(())
    }
}

/// Converts DynamoDB JSON into preferences.
fn item_to_preferences(item: &Value) -> Preferences {
    Preferences {
        show_category_counts: attr_bool(item, "show_category_counts").unwrap_or(false),
        delivery_mode: attr_string(item, "delivery_mode")
            .and_then(|value| DeliveryMode::parse(&value))
            .unwrap_or_default(),
        file_type: attr_string(item, "file_type")
            .and_then(|value| FileType::parse(&value))
            .unwrap_or_default(),
        extension: attr_string(item, "extension").filter(|value| !value.is_empty()),
        favorite_categories: attr_string_list(item, "favorite_categories"),
        blacklist_categories: attr_string_list(item, "blacklist_categories"),
        blacklist_uploaders: attr_string_list(item, "blacklist_uploaders"),
        show_sha1: attr_bool(item, "show_sha1").unwrap_or(false),
        show_file_size: attr_bool(item, "show_file_size").unwrap_or(false),
        show_preview_metadata: attr_bool(item, "show_preview_metadata").unwrap_or(true),
        rich_image_previews: attr_bool(item, "rich_image_previews").unwrap_or(false),
        pagination_enabled: attr_bool(item, "pagination_enabled").unwrap_or(true),
        inline_result_count: attr_usize(item, "inline_result_count")
            .map(normalize_inline_result_count)
            .unwrap_or(DEFAULT_INLINE_RESULT_COUNT),
        pdf_mode: attr_string(item, "pdf_mode")
            .and_then(|value| DocumentPageMode::parse(&value))
            .unwrap_or_default(),
        djvu_mode: attr_string(item, "djvu_mode")
            .and_then(|value| DocumentPageMode::parse(&value))
            .unwrap_or_default(),
    }
}

/// Converts preferences into DynamoDB JSON.
fn preferences_to_item(telegram_user_id: i64, preferences: &Preferences) -> Value {
    json!({
        "pk": {"S": format!("USER#{telegram_user_id}")},
        "sk": {"S": "PREFERENCES"},
        "show_category_counts": {"BOOL": preferences.show_category_counts},
        "delivery_mode": {"S": preferences.delivery_mode.as_pref_value()},
        "file_type": {"S": preferences.file_type.as_pref_value()},
        "extension": {"S": preferences.extension.clone().unwrap_or_default()},
        "favorite_categories": {"L": preferences.favorite_categories.iter().map(|value| json!({"S": value})).collect::<Vec<_>>()},
        "blacklist_categories": {"L": preferences.blacklist_categories.iter().map(|value| json!({"S": value})).collect::<Vec<_>>()},
        "blacklist_uploaders": {"L": preferences.blacklist_uploaders.iter().map(|value| json!({"S": value})).collect::<Vec<_>>()},
        "show_sha1": {"BOOL": preferences.show_sha1},
        "show_file_size": {"BOOL": preferences.show_file_size},
        "show_preview_metadata": {"BOOL": preferences.show_preview_metadata},
        "rich_image_previews": {"BOOL": preferences.rich_image_previews},
        "pagination_enabled": {"BOOL": preferences.pagination_enabled},
        "inline_result_count": {"N": preferences.normalized_inline_result_count().to_string()},
        "pdf_mode": {"S": preferences.pdf_mode.as_pref_value()},
        "djvu_mode": {"S": preferences.djvu_mode.as_pref_value()},
    })
}

/// Reads a DynamoDB string attribute.
fn attr_string(item: &Value, key: &str) -> Option<String> {
    item.get(key)?.get("S")?.as_str().map(str::to_string)
}

/// Reads a DynamoDB bool attribute.
fn attr_bool(item: &Value, key: &str) -> Option<bool> {
    item.get(key)?.get("BOOL")?.as_bool()
}

/// Reads a DynamoDB unsigned integer attribute.
fn attr_usize(item: &Value, key: &str) -> Option<usize> {
    item.get(key)?.get("N")?.as_str()?.parse::<usize>().ok()
}

/// Reads a DynamoDB string list attribute.
fn attr_string_list(item: &Value, key: &str) -> Vec<String> {
    item.get(key)
        .and_then(|value| value.get("L"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("S").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{PreferenceStore, item_to_preferences, preferences_to_item};
    use crate::config::Config;
    use crate::models::{
        DEFAULT_INLINE_RESULT_COUNT, DeliveryMode, DocumentPageMode, FileType, Preferences,
    };
    use serde_json::json;

    fn test_config(stateless_mode: bool, dynamodb_table: Option<&str>) -> Config {
        Config {
            telegram_bot_token: None,
            telegram_webhook_secret: None,
            admin_user_ids: Vec::new(),
            github_url: "https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons".into(),
            aws_region: "us-east-1".into(),
            lambda_function_name: "telegram-wikimedia-commons-bot".into(),
            dynamodb_table: dynamodb_table.map(str::to_string),
            stateless_mode,
            max_file_bytes: 50 * 1024 * 1024,
            user_agent: "test-agent".into(),
            commons_api_url: "https://commons.wikimedia.org/w/api.php".into(),
            commons_auth_cookie_ssm_parameter: None,
            enable_test_endpoint: false,
        }
    }

    #[test]
    fn round_trips_preferences_through_dynamodb_json() {
        let prefs = Preferences {
            show_category_counts: true,
            delivery_mode: DeliveryMode::Images20,
            file_type: FileType::Audio,
            extension: Some("flac".into()),
            favorite_categories: vec!["Minsk".into()],
            blacklist_categories: vec!["Bad category".into()],
            blacklist_uploaders: vec!["Example_User".into()],
            show_sha1: true,
            show_file_size: true,
            show_preview_metadata: false,
            rich_image_previews: true,
            pagination_enabled: false,
            inline_result_count: 10,
            pdf_mode: DocumentPageMode::RenderedPages,
            djvu_mode: DocumentPageMode::RenderedPages,
        };
        let item = preferences_to_item(42, &prefs);
        assert_eq!(item["pk"]["S"], "USER#42");
        assert_eq!(item["sk"]["S"], "PREFERENCES");
        let parsed = item_to_preferences(&item);
        assert!(parsed.show_category_counts);
        assert_eq!(parsed.delivery_mode, DeliveryMode::Images20);
        assert_eq!(parsed.file_type, FileType::Audio);
        assert_eq!(parsed.extension, Some("flac".into()));
        assert_eq!(parsed.favorite_categories, vec!["Minsk"]);
        assert_eq!(parsed.blacklist_categories, vec!["Bad category"]);
        assert_eq!(parsed.blacklist_uploaders, vec!["Example_User"]);
        assert!(parsed.show_sha1);
        assert!(parsed.show_file_size);
        assert!(!parsed.show_preview_metadata);
        assert!(parsed.rich_image_previews);
        assert!(!parsed.pagination_enabled);
        assert_eq!(parsed.inline_result_count, 10);
        assert_eq!(parsed.pdf_mode, DocumentPageMode::RenderedPages);
        assert_eq!(parsed.djvu_mode, DocumentPageMode::RenderedPages);
    }

    #[test]
    fn old_preferences_default_to_new_enabled_flags() {
        let item = json!({
            "pk": {"S": "USER#42"},
            "sk": {"S": "PREFERENCES"}
        });
        let preferences = item_to_preferences(&item);
        assert!(preferences.show_preview_metadata);
        assert!(!preferences.rich_image_previews);
        assert!(preferences.pagination_enabled);
        assert_eq!(preferences.inline_result_count, DEFAULT_INLINE_RESULT_COUNT);
    }

    #[test]
    fn malformed_dynamodb_attributes_fall_back_safely() {
        let item = json!({
            "show_category_counts": {"S": "true"},
            "delivery_mode": {"S": "bad"},
            "file_type": {"S": "bad"},
            "extension": {"S": ""},
            "favorite_categories": {"L": [{"S": "Minsk"}, {"N": "1"}]},
            "blacklist_categories": {"S": "not-a-list"},
            "blacklist_uploaders": {"L": [{"S": "Example_User"}]},
            "show_sha1": {"BOOL": true},
            "show_file_size": {"BOOL": true},
            "show_preview_metadata": {"BOOL": false},
            "rich_image_previews": {"BOOL": true},
            "pagination_enabled": {"BOOL": false},
            "inline_result_count": {"N": "17"},
            "pdf_mode": {"S": "bad"},
            "djvu_mode": {"S": "rendered"}
        });

        let preferences = item_to_preferences(&item);

        assert!(!preferences.show_category_counts);
        assert_eq!(preferences.delivery_mode, DeliveryMode::Buttons);
        assert_eq!(preferences.file_type, FileType::All);
        assert_eq!(preferences.extension, None);
        assert_eq!(preferences.favorite_categories, vec!["Minsk"]);
        assert!(preferences.blacklist_categories.is_empty());
        assert_eq!(preferences.blacklist_uploaders, vec!["Example_User"]);
        assert!(preferences.show_sha1);
        assert!(preferences.show_file_size);
        assert!(!preferences.show_preview_metadata);
        assert!(preferences.rich_image_previews);
        assert!(!preferences.pagination_enabled);
        assert_eq!(preferences.inline_result_count, DEFAULT_INLINE_RESULT_COUNT);
        assert_eq!(preferences.pdf_mode, DocumentPageMode::Original);
        assert_eq!(preferences.djvu_mode, DocumentPageMode::RenderedPages);
    }

    #[tokio::test]
    async fn stateless_store_does_not_persist_preferences() {
        let store = PreferenceStore::new(&test_config(true, Some("preferences")));
        let changed = Preferences {
            show_sha1: true,
            ..Preferences::default()
        };

        store.put(42, &changed).await.unwrap();

        assert_eq!(store.get(42).await, Preferences::default());
    }

    #[tokio::test]
    async fn store_without_table_does_not_require_aws_credentials() {
        let store = PreferenceStore::new(&test_config(false, None));
        let changed = Preferences {
            delivery_mode: DeliveryMode::Links10,
            ..Preferences::default()
        };

        store.put(42, &changed).await.unwrap();

        assert_eq!(store.get(42).await, Preferences::default());
    }
}
