use crate::aws::AwsJsonClient;
use crate::config::Config;
use anyhow::Result;
use once_cell::sync::Lazy;
use serde_json::json;
use std::collections::HashMap;
use time::OffsetDateTime;
use tokio::sync::RwLock;

static RAM_SEEN: Lazy<RwLock<HashMap<String, i64>>> = Lazy::new(|| RwLock::new(HashMap::new()));

/// Stores short-lived Telegram update reservations to suppress webhook retries.
#[derive(Clone)]
pub struct IdempotencyStore {
    table_name: Option<String>,
    stateless_mode: bool,
    aws: AwsJsonClient,
}

impl IdempotencyStore {
    /// Creates an idempotency store from runtime configuration.
    pub fn new(config: &Config) -> Self {
        Self {
            table_name: config.dynamodb_table.clone(),
            stateless_mode: config.stateless_mode,
            aws: AwsJsonClient::new(config.aws_region.clone()),
        }
    }

    /// Reserves a key and returns false when it was already seen and not expired.
    pub async fn reserve(&self, key: &str, retention_seconds: i64) -> Result<bool> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let expires_at = now.saturating_add(retention_seconds.max(1));
        if self.stateless_mode || self.table_name.is_none() || !self.aws.has_credentials() {
            return reserve_in_ram(key, now, expires_at).await;
        }
        self.reserve_in_dynamodb(key, now, expires_at).await
    }

    /// Marks a reserved key as successfully processed while keeping its expiry.
    pub async fn mark_done(&self, key: &str, retention_seconds: i64) -> Result<()> {
        let expires_at = OffsetDateTime::now_utc()
            .unix_timestamp()
            .saturating_add(retention_seconds.max(1));
        if self.stateless_mode || self.table_name.is_none() || !self.aws.has_credentials() {
            return Ok(());
        }
        self.put_dynamodb_item(key, "done", expires_at, None)
            .await?;
        Ok(())
    }

    /// Reserves a key in DynamoDB with an atomic condition.
    async fn reserve_in_dynamodb(&self, key: &str, now: i64, expires_at: i64) -> Result<bool> {
        let result = self
            .put_dynamodb_item(
                key,
                "processing",
                expires_at,
                Some(json!({
                    "ConditionExpression": "attribute_not_exists(pk) OR expires_at < :now",
                    "ExpressionAttributeValues": {
                        ":now": {"N": now.to_string()}
                    }
                })),
            )
            .await;
        match result {
            Ok(()) => Ok(true),
            Err(error) if is_conditional_check_failed(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Writes one idempotency item to DynamoDB.
    async fn put_dynamodb_item(
        &self,
        key: &str,
        status: &str,
        expires_at: i64,
        extra: Option<serde_json::Value>,
    ) -> Result<()> {
        let table = self.table_name.as_ref().expect("checked by caller");
        let mut body = json!({
            "TableName": table,
            "Item": {
                "pk": {"S": key},
                "sk": {"S": "IDEMPOTENCY"},
                "status": {"S": status},
                "expires_at": {"N": expires_at.to_string()}
            }
        });
        if let Some(extra) = extra
            && let Some(map) = extra.as_object()
        {
            for (key, value) in map {
                body[key] = value.clone();
            }
        }
        self.aws
            .post_json("dynamodb", "DynamoDB_20120810.PutItem", body)
            .await?;
        Ok(())
    }
}

/// Returns true for DynamoDB conditional-write failures.
fn is_conditional_check_failed(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("ConditionalCheckFailedException")
}

/// Reserves a key in the warm Lambda RAM cache.
async fn reserve_in_ram(key: &str, now: i64, expires_at: i64) -> Result<bool> {
    let mut seen = RAM_SEEN.write().await;
    seen.retain(|_, expiry| *expiry >= now);
    if seen.get(key).is_some_and(|expiry| *expiry >= now) {
        return Ok(false);
    }
    seen.insert(key.to_string(), expires_at);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{RAM_SEEN, is_conditional_check_failed, reserve_in_ram};

    #[tokio::test]
    async fn ram_reservation_suppresses_unexpired_duplicates() {
        RAM_SEEN.write().await.clear();

        assert!(reserve_in_ram("telegram:update:1", 100, 200).await.unwrap());
        assert!(!reserve_in_ram("telegram:update:1", 101, 201).await.unwrap());
        assert!(reserve_in_ram("telegram:update:1", 201, 301).await.unwrap());

        assert!(reserve_in_ram("old", 100, 101).await.unwrap());
        assert!(reserve_in_ram("new", 102, 200).await.unwrap());

        let seen = RAM_SEEN.read().await;
        assert!(!seen.contains_key("old"));
        assert!(seen.contains_key("new"));
    }

    #[test]
    fn detects_dynamodb_conditional_check_errors() {
        let error = anyhow::anyhow!(
            "{}",
            r#"AWS dynamodb DynamoDB_20120810.PutItem failed with HTTP 400 Bad Request: {"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException"}"#
        );

        assert!(is_conditional_check_failed(&error));
        assert!(!is_conditional_check_failed(&anyhow::anyhow!("other")));
    }
}
