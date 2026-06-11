use crate::models::DEFAULT_MAX_FILE_BYTES;
use std::env;

/// Runtime configuration loaded from environment variables.
#[derive(Clone, Debug)]
pub struct Config {
    /// Telegram bot token.
    pub telegram_bot_token: Option<String>,
    /// Telegram webhook secret token.
    pub telegram_webhook_secret: Option<String>,
    /// Comma-separated admin Telegram user ids.
    pub admin_user_ids: Vec<i64>,
    /// GitHub repository URL shown in `/help`.
    pub github_url: String,
    /// AWS region for Lambda, DynamoDB, and CloudWatch.
    pub aws_region: String,
    /// Lambda function name used by scripts and admin stats.
    pub lambda_function_name: String,
    /// DynamoDB table name for preferences.
    pub dynamodb_table: Option<String>,
    /// If true, DynamoDB preferences are disabled.
    pub stateless_mode: bool,
    /// Maximum Telegram-returnable file size.
    pub max_file_bytes: u64,
    /// Commons User-Agent.
    pub user_agent: String,
    /// Commons API endpoint.
    pub commons_api_url: String,
    /// Optional SSM SecureString parameter containing a Pywikibot LWP cookie jar.
    pub commons_auth_cookie_ssm_parameter: Option<String>,
    /// Enables the authenticated Lambda test endpoint for CI.
    pub enable_test_endpoint: bool,
}

impl Config {
    /// Loads configuration from environment variables with hobby-project defaults.
    pub fn from_env() -> Self {
        let project_name =
            env::var("PROJECT_NAME").unwrap_or_else(|_| "telegram-wikimedia-commons-bot".into());
        let aws_region = env::var("AWS_REGION")
            .or_else(|_| env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".into());
        let max_file_bytes = env::var("MAX_FILE_MB")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(|mb| mb * 1024 * 1024)
            .unwrap_or(DEFAULT_MAX_FILE_BYTES);

        Self {
            telegram_bot_token: env::var("TELEGRAM_BOT_TOKEN").ok(),
            telegram_webhook_secret: env::var("TELEGRAM_WEBHOOK_SECRET").ok(),
            admin_user_ids: parse_admin_ids(&env::var("ADMIN_TELEGRAM_USER_IDS").unwrap_or_default()),
            github_url: env::var("GITHUB_URL").unwrap_or_else(|_| {
                "https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons".into()
            }),
            aws_region,
            lambda_function_name: env::var("LAMBDA_FUNCTION_NAME").unwrap_or(project_name),
            dynamodb_table: env::var("DYNAMODB_TABLE").ok().filter(|value| !value.is_empty()),
            stateless_mode: env::var("STATELESS_MODE")
                .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes"))
                .unwrap_or(false),
            max_file_bytes,
            user_agent: env::var("COMMONS_USER_AGENT").unwrap_or_else(|_| {
                "telegram-wikimedia-commons-bot/0.1 (https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons)".into()
            }),
            commons_api_url: env::var("COMMONS_API_URL")
                .unwrap_or_else(|_| "https://commons.wikimedia.org/w/api.php".into()),
            commons_auth_cookie_ssm_parameter: env::var("COMMONS_AUTH_COOKIE_SSM_PARAMETER")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            enable_test_endpoint: env::var("ENABLE_TEST_ENDPOINT")
                .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes"))
                .unwrap_or(false),
        }
    }

    /// Returns true if a Telegram user id belongs to an administrator.
    pub fn is_admin(&self, user_id: i64) -> bool {
        self.admin_user_ids.contains(&user_id)
    }
}

/// Parses comma-separated Telegram numeric user ids.
fn parse_admin_ids(value: &str) -> Vec<i64> {
    value
        .split(',')
        .filter_map(|part| part.trim().parse::<i64>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_admin_ids;

    #[test]
    fn parses_admin_ids() {
        assert_eq!(parse_admin_ids("1, 2, bad,3"), vec![1, 2, 3]);
    }
}
