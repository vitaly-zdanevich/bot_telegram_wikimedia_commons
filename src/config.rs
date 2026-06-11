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
        Self::from_env_lookup(|key| env::var(key).ok())
    }

    /// Builds configuration from a supplied environment lookup function.
    fn from_env_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Self {
        let project_name =
            lookup("PROJECT_NAME").unwrap_or_else(|| "telegram-wikimedia-commons-bot".into());
        let aws_region = lookup("AWS_REGION")
            .or_else(|| lookup("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|| "us-east-1".into());
        let max_file_bytes = lookup("MAX_FILE_MB")
            .and_then(|value| value.parse::<u64>().ok())
            .map(|mb| mb * 1024 * 1024)
            .unwrap_or(DEFAULT_MAX_FILE_BYTES);

        Self {
            telegram_bot_token: lookup("TELEGRAM_BOT_TOKEN"),
            telegram_webhook_secret: lookup("TELEGRAM_WEBHOOK_SECRET"),
            admin_user_ids: parse_admin_ids(
                &lookup("ADMIN_TELEGRAM_USER_IDS").unwrap_or_default(),
            ),
            github_url: lookup("GITHUB_URL").unwrap_or_else(|| {
                "https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons".into()
            }),
            aws_region,
            lambda_function_name: lookup("LAMBDA_FUNCTION_NAME").unwrap_or(project_name),
            dynamodb_table: lookup("DYNAMODB_TABLE").filter(|value| !value.is_empty()),
            stateless_mode: lookup("STATELESS_MODE")
                .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes"))
                .unwrap_or(false),
            max_file_bytes,
            user_agent: lookup("COMMONS_USER_AGENT").unwrap_or_else(|| {
                "telegram-wikimedia-commons-bot/0.1 (https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons)".into()
            }),
            commons_api_url: lookup("COMMONS_API_URL")
                .unwrap_or_else(|| "https://commons.wikimedia.org/w/api.php".into()),
            commons_auth_cookie_ssm_parameter: lookup("COMMONS_AUTH_COOKIE_SSM_PARAMETER")
                .filter(|value| !value.trim().is_empty()),
            enable_test_endpoint: lookup("ENABLE_TEST_ENDPOINT")
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
    use super::{Config, parse_admin_ids};
    use crate::models::DEFAULT_MAX_FILE_BYTES;
    use std::collections::HashMap;

    fn config_from_pairs(pairs: &[(&str, &str)]) -> Config {
        let values = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<HashMap<_, _>>();
        Config::from_env_lookup(|key| values.get(key).cloned())
    }

    #[test]
    fn parses_admin_ids() {
        assert_eq!(parse_admin_ids("1, 2, bad,3"), vec![1, 2, 3]);
    }

    #[test]
    fn loads_defaults_when_environment_is_absent() {
        let config = config_from_pairs(&[]);

        assert_eq!(config.telegram_bot_token, None);
        assert_eq!(config.telegram_webhook_secret, None);
        assert!(config.admin_user_ids.is_empty());
        assert_eq!(
            config.github_url,
            "https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons"
        );
        assert_eq!(config.aws_region, "us-east-1");
        assert_eq!(
            config.lambda_function_name,
            "telegram-wikimedia-commons-bot"
        );
        assert_eq!(config.dynamodb_table, None);
        assert!(!config.stateless_mode);
        assert_eq!(config.max_file_bytes, DEFAULT_MAX_FILE_BYTES);
        assert_eq!(
            config.commons_api_url,
            "https://commons.wikimedia.org/w/api.php"
        );
        assert_eq!(config.commons_auth_cookie_ssm_parameter, None);
        assert!(!config.enable_test_endpoint);
    }

    #[test]
    fn loads_explicit_environment_values() {
        let config = config_from_pairs(&[
            ("TELEGRAM_BOT_TOKEN", "token"),
            ("TELEGRAM_WEBHOOK_SECRET", "secret"),
            ("ADMIN_TELEGRAM_USER_IDS", "42,bad,7"),
            ("GITHUB_URL", "https://example.test/repo"),
            ("AWS_REGION", "eu-central-1"),
            ("PROJECT_NAME", "project-default"),
            ("LAMBDA_FUNCTION_NAME", "lambda-name"),
            ("DYNAMODB_TABLE", "preferences"),
            ("STATELESS_MODE", "true"),
            ("MAX_FILE_MB", "3"),
            ("COMMONS_USER_AGENT", "custom-agent"),
            ("COMMONS_API_URL", "https://commons.example.test/api.php"),
            ("COMMONS_AUTH_COOKIE_SSM_PARAMETER", "/commons/cookies"),
            ("ENABLE_TEST_ENDPOINT", "yes"),
        ]);

        assert_eq!(config.telegram_bot_token.as_deref(), Some("token"));
        assert_eq!(config.telegram_webhook_secret.as_deref(), Some("secret"));
        assert_eq!(config.admin_user_ids, vec![42, 7]);
        assert_eq!(config.github_url, "https://example.test/repo");
        assert_eq!(config.aws_region, "eu-central-1");
        assert_eq!(config.lambda_function_name, "lambda-name");
        assert_eq!(config.dynamodb_table.as_deref(), Some("preferences"));
        assert!(config.stateless_mode);
        assert_eq!(config.max_file_bytes, 3 * 1024 * 1024);
        assert_eq!(config.user_agent, "custom-agent");
        assert_eq!(
            config.commons_api_url,
            "https://commons.example.test/api.php"
        );
        assert_eq!(
            config.commons_auth_cookie_ssm_parameter.as_deref(),
            Some("/commons/cookies")
        );
        assert!(config.enable_test_endpoint);
    }

    #[test]
    fn falls_back_to_aws_default_region_and_project_lambda_name() {
        let config = config_from_pairs(&[
            ("AWS_DEFAULT_REGION", "us-west-2"),
            ("PROJECT_NAME", "project-name"),
            ("DYNAMODB_TABLE", ""),
            ("COMMONS_AUTH_COOKIE_SSM_PARAMETER", " "),
            ("STATELESS_MODE", "0"),
            ("ENABLE_TEST_ENDPOINT", "false"),
        ]);

        assert_eq!(config.aws_region, "us-west-2");
        assert_eq!(config.lambda_function_name, "project-name");
        assert_eq!(config.dynamodb_table, None);
        assert_eq!(config.commons_auth_cookie_ssm_parameter, None);
        assert!(!config.stateless_mode);
        assert!(!config.enable_test_endpoint);
    }

    #[test]
    fn checks_admin_ids_from_config() {
        let config = config_from_pairs(&[("ADMIN_TELEGRAM_USER_IDS", "42,7")]);

        assert!(config.is_admin(42));
        assert!(config.is_admin(7));
        assert!(!config.is_admin(8));
    }
}
