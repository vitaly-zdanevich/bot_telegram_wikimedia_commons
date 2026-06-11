use crate::aws::AwsJsonClient;
use crate::commons_cache::{CommonsCacheStats, cache_stats};
use crate::config::Config;
use anyhow::Result;
use html_escape::encode_text;
use quick_xml::{Reader, events::Event};
use serde_json::json;
use time::{Duration, OffsetDateTime, Weekday, format_description::well_known::Rfc3339};

/// Static AWS free-tier constants used by the CLI and admin `/stat` output.
pub const LAMBDA_FREE_REQUESTS_PER_MONTH: f64 = 1_000_000.0;
pub const LAMBDA_FREE_GB_SECONDS_PER_MONTH: f64 = 400_000.0;
pub const DYNAMODB_FREE_STORAGE_GB: f64 = 25.0;

/// A compact operational stats snapshot.
#[derive(Clone, Debug, Default)]
pub struct StatsSnapshot {
    /// Lambda invocations in the last 24 hours.
    pub invocations_24h: u64,
    /// Lambda invocations in the last 7 days.
    pub invocations_7d: u64,
    /// Lambda errors in the last 24 hours.
    pub errors_24h: u64,
    /// Lambda errors in the last 7 days.
    pub errors_7d: u64,
    /// Lambda errors in the last month.
    pub errors_month: u64,
    /// Minimum Lambda duration in ms.
    pub min_duration_ms: f64,
    /// Average Lambda duration in ms.
    pub avg_duration_ms: f64,
    /// Maximum Lambda duration in ms.
    pub max_duration_ms: f64,
    /// DynamoDB table size in bytes.
    pub dynamodb_size_bytes: u64,
    /// Lambda invocations for each of the previous seven UTC days.
    pub daily_invocations: [u64; 7],
    /// Labels for the daily invocation chart.
    pub daily_labels: [String; 7],
    /// Warm Lambda Commons cache usage.
    pub commons_cache: CommonsCacheStats,
}

impl StatsSnapshot {
    /// Renders a Telegram-safe text dashboard with AWS documentation links.
    pub fn render_text(&self, config: &Config) -> String {
        let gb_seconds = (self.invocations_7d as f64)
            * (self.avg_duration_ms / 1000.0)
            * (lambda_memory_gb_from_env());
        let request_pct = percent(self.invocations_7d as f64, LAMBDA_FREE_REQUESTS_PER_MONTH);
        let duration_pct = percent(gb_seconds, LAMBDA_FREE_GB_SECONDS_PER_MONTH);
        let dynamodb_gb = self.dynamodb_size_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
        let dynamodb_pct = percent(dynamodb_gb, DYNAMODB_FREE_STORAGE_GB);
        let summary = render_summary_block(self);
        let cache = render_cache_block(&self.commons_cache);
        let cloudwatch_url = format!(
            "https://{}.console.aws.amazon.com/cloudwatch/home?region={}#logsV2:log-groups/log-group/$252Faws$252Flambda$252F{}",
            config.aws_region, config.aws_region, config.lambda_function_name
        );
        let dynamodb_url = config
            .dynamodb_table
            .as_ref()
            .map(|table| {
                format!(
                    "https://{}.console.aws.amazon.com/dynamodbv2/home?region={}#table?name={table}",
                    config.aws_region, config.aws_region
                )
            })
            .unwrap_or_else(|| "DynamoDB is disabled in stateless mode".into());
        let daily_chart = if self.daily_labels.iter().any(|label| !label.is_empty()) {
            let labels = [
                self.daily_labels[0].as_str(),
                self.daily_labels[1].as_str(),
                self.daily_labels[2].as_str(),
                self.daily_labels[3].as_str(),
                self.daily_labels[4].as_str(),
                self.daily_labels[5].as_str(),
                self.daily_labels[6].as_str(),
            ];
            format!(
                "\nCalls per day:\n<pre>{}</pre>\n",
                encode_text(&render_week_chart(&self.daily_invocations, &labels))
            )
        } else {
            String::new()
        };

        format!(
            "Stats\n\n<pre>{}</pre>\n\nCache\n<pre>{}</pre>\n{}\nFree tier use estimate, based on last 7 days as a rough monthly signal:\nLambda requests: {:.1}%\nLambda duration: {:.1}% ({:.0} GB-s)\nDynamoDB storage: {:.3}% ({:.4} GB)\n\nAWS Lambda free tier:\nhttps://aws.amazon.com/lambda/pricing/\n\nDynamoDB free tier: 25 GB, 25 RCU/WCU:\nhttps://aws.amazon.com/dynamodb/pricing/\n\nCloudWatch:\n{}\n\nDynamoDB:\n{}",
            encode_text(&summary),
            encode_text(&cache),
            daily_chart,
            request_pct,
            duration_pct,
            gb_seconds,
            dynamodb_pct,
            dynamodb_gb,
            cloudwatch_url,
            dynamodb_url
        )
    }
}

/// Renders fixed-width summary rows for Telegram monospace display.
fn render_summary_block(stats: &StatsSnapshot) -> String {
    format!(
        "Calls\n24h {:>8}\n7d  {:>8}\n\nErrors\n24h {:>8}\n7d  {:>8}\n30d {:>8}\n\nDuration\nmin {:>6.0} ms\navg {:>6.0} ms\nmax {:>6.0} ms",
        stats.invocations_24h,
        stats.invocations_7d,
        stats.errors_24h,
        stats.errors_7d,
        stats.errors_month,
        stats.min_duration_ms,
        stats.avg_duration_ms,
        stats.max_duration_ms,
    )
}

/// Renders fixed-width cache rows for Telegram monospace display.
fn render_cache_block(cache: &CommonsCacheStats) -> String {
    let ram_pct = percent(
        cache.ram_file_bytes_bytes as f64,
        cache.ram_file_bytes_max_bytes as f64,
    );
    let tmp_pct = percent(
        cache.tmp_file_bytes_bytes as f64,
        cache.tmp_file_bytes_max_bytes as f64,
    );
    format!(
        "RAM meta {:>8}\n\nRAM files\nused    {:>8}\nlimit   {:>8}\npct     {:>7.1}%\nentries {:>8}\n\n/tmp files\nused    {:>8}\nlimit   {:>8}\npct     {:>7.1}%\nentries {:>8}\n\nSearch   {:>8}\nCategory {:>8}",
        cache.ram_metadata_entries(),
        format_bytes(cache.ram_file_bytes_bytes as u64),
        format_bytes(cache.ram_file_bytes_max_bytes as u64),
        ram_pct,
        cache.ram_file_bytes_entries,
        format_bytes(cache.tmp_file_bytes_bytes),
        format_bytes(cache.tmp_file_bytes_max_bytes),
        tmp_pct,
        cache.tmp_file_bytes_entries,
        cache.file_search_entries,
        cache.category_search_entries
            + cache.category_info_entries
            + cache.category_file_count_entries,
    )
}

/// Loads live Lambda and DynamoDB stats through minimal signed AWS HTTP calls.
pub async fn load_admin_stats(config: &Config) -> Result<StatsSnapshot> {
    let commons_cache = cache_stats().await;
    let aws = AwsJsonClient::new(config.aws_region.clone());
    if !aws.has_credentials() {
        return Ok(StatsSnapshot {
            commons_cache,
            ..StatsSnapshot::default()
        });
    }

    let now = OffsetDateTime::now_utc();
    let start_24h = now - Duration::hours(24);
    let start_7d = now - Duration::days(7);
    let start_month = now - Duration::days(30);
    let invocations_24h = metric_sum(&aws, config, "Invocations", start_24h, now, 3600).await?;
    let invocations_7d = metric_sum(&aws, config, "Invocations", start_7d, now, 86400).await?;
    let errors_24h = metric_sum(&aws, config, "Errors", start_24h, now, 3600).await?;
    let errors_7d = metric_sum(&aws, config, "Errors", start_7d, now, 86400).await?;
    let errors_month = metric_sum(&aws, config, "Errors", start_month, now, 86400).await?;
    let min_duration_ms = duration_stat(&aws, config, "Minimum", start_7d, now).await?;
    let avg_duration_ms = duration_stat(&aws, config, "Average", start_7d, now).await?;
    let max_duration_ms = duration_stat(&aws, config, "Maximum", start_7d, now).await?;
    let dynamodb_size_bytes = dynamodb_table_size(&aws, config).await.unwrap_or(0);
    let (daily_invocations, daily_labels) = daily_invocation_chart(&aws, config, now).await?;

    Ok(StatsSnapshot {
        invocations_24h,
        invocations_7d,
        errors_24h,
        errors_7d,
        errors_month,
        min_duration_ms,
        avg_duration_ms,
        max_duration_ms,
        dynamodb_size_bytes,
        daily_invocations,
        daily_labels,
        commons_cache,
    })
}

/// Reads a CloudWatch metric and returns the sum of all returned datapoints.
async fn metric_sum(
    aws: &AwsJsonClient,
    config: &Config,
    metric: &str,
    start: OffsetDateTime,
    end: OffsetDateTime,
    period: u64,
) -> Result<u64> {
    let values = cloudwatch_metric_values(aws, config, metric, "Sum", start, end, period).await?;
    Ok(values.iter().sum::<f64>().round() as u64)
}

/// Reads one duration statistic from CloudWatch, averaged across datapoints.
async fn duration_stat(
    aws: &AwsJsonClient,
    config: &Config,
    statistic: &str,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<f64> {
    let values =
        cloudwatch_metric_values(aws, config, "Duration", statistic, start, end, 86400).await?;
    if values.is_empty() {
        Ok(0.0)
    } else {
        Ok(values.iter().sum::<f64>() / values.len() as f64)
    }
}

/// Fetches CloudWatch metric datapoint values through the Query API.
async fn cloudwatch_metric_values(
    aws: &AwsJsonClient,
    config: &Config,
    metric: &str,
    statistic: &str,
    start: OffsetDateTime,
    end: OffsetDateTime,
    period: u64,
) -> Result<Vec<f64>> {
    let params = vec![
        ("Action".into(), "GetMetricStatistics".into()),
        ("Version".into(), "2010-08-01".into()),
        ("Namespace".into(), "AWS/Lambda".into()),
        ("MetricName".into(), metric.into()),
        ("Dimensions.member.1.Name".into(), "FunctionName".into()),
        (
            "Dimensions.member.1.Value".into(),
            config.lambda_function_name.clone(),
        ),
        ("StartTime".into(), start.format(&Rfc3339)?),
        ("EndTime".into(), end.format(&Rfc3339)?),
        ("Period".into(), period.to_string()),
        ("Statistics.member.1".into(), statistic.into()),
    ];
    let xml = aws.post_query("monitoring", &params).await?;
    parse_metric_values(&xml, statistic)
}

/// Reads DynamoDB table size in bytes, returning zero when preferences are disabled.
async fn dynamodb_table_size(aws: &AwsJsonClient, config: &Config) -> Result<u64> {
    let Some(table) = &config.dynamodb_table else {
        return Ok(0);
    };
    let value = aws
        .post_json(
            "dynamodb",
            "DynamoDB_20120810.DescribeTable",
            json!({ "TableName": table }),
        )
        .await?;
    Ok(value["Table"]["TableSizeBytes"].as_u64().unwrap_or(0))
}

/// Builds the seven-column invocation chart for previous UTC days.
async fn daily_invocation_chart(
    aws: &AwsJsonClient,
    config: &Config,
    now: OffsetDateTime,
) -> Result<([u64; 7], [String; 7])> {
    let mut values = [0_u64; 7];
    let mut labels = std::array::from_fn(|_| String::new());
    for (index, days_ago) in (0_i64..=6).rev().enumerate() {
        let day = (now - Duration::days(days_ago)).date();
        let start = day.midnight().assume_utc();
        let end = start + Duration::days(1);
        values[index] = metric_sum(aws, config, "Invocations", start, end, 86400).await?;
        labels[index] = weekday_label(day.weekday()).to_string();
    }
    Ok((values, labels))
}

/// Parses metric values from AWS CloudWatch XML responses.
fn parse_metric_values(xml: &str, metric_tag: &str) -> Result<Vec<f64>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut values = Vec::new();
    let mut current_tag = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Start(element) => current_tag = element.name().as_ref().to_vec(),
            Event::Text(text) if current_tag.as_slice() == metric_tag.as_bytes() => {
                if let Ok(value) = text.decode()?.parse::<f64>() {
                    values.push(value);
                }
            }
            Event::End(_) => current_tag.clear(),
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(values)
}

/// Returns a short English weekday label for compact charts.
fn weekday_label(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Monday => "Mon",
        Weekday::Tuesday => "Tue",
        Weekday::Wednesday => "Wed",
        Weekday::Thursday => "Thu",
        Weekday::Friday => "Fri",
        Weekday::Saturday => "Sat",
        Weekday::Sunday => "Sun",
    }
}

/// Returns the Lambda memory size in GB from env for rough free-tier math.
fn lambda_memory_gb_from_env() -> f64 {
    std::env::var("AWS_LAMBDA_FUNCTION_MEMORY_SIZE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .map(|mb| mb / 1024.0)
        .unwrap_or(1.0)
}

/// Calculates a percent while avoiding division-by-zero panics.
fn percent(value: f64, limit: f64) -> f64 {
    if limit <= 0.0 {
        0.0
    } else {
        (value / limit) * 100.0
    }
}

/// Formats bytes for compact stat rows.
fn format_bytes(bytes: u64) -> String {
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

/// Renders a 7-column ASCII chart.
pub fn render_week_chart(values: &[u64; 7], labels: &[&str; 7]) -> String {
    let max = values.iter().copied().max().unwrap_or(0).max(1);
    labels
        .iter()
        .zip(values)
        .map(|(label, value)| {
            let width = ((*value as f64 / max as f64) * 20.0).round() as usize;
            format!("{label} {:>6} {}", value, "#".repeat(width.max(1)))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{
        CommonsCacheStats, StatsSnapshot, format_bytes, parse_metric_values, percent,
        render_cache_block, render_summary_block, render_week_chart, weekday_label,
    };
    use crate::config::Config;
    use time::Weekday;

    #[test]
    fn renders_summary_block_for_monospace() {
        let stats = StatsSnapshot {
            invocations_24h: 267,
            invocations_7d: 267,
            errors_24h: 81,
            errors_7d: 81,
            errors_month: 1,
            min_duration_ms: 1.0,
            avg_duration_ms: 1569.0,
            max_duration_ms: 21174.0,
            ..StatsSnapshot::default()
        };
        let block = render_summary_block(&stats);
        assert_eq!(
            block,
            "Calls\n24h      267\n7d       267\n\nErrors\n24h       81\n7d        81\n30d        1\n\nDuration\nmin      1 ms\navg   1569 ms\nmax  21174 ms"
        );
        assert!(block.lines().all(|line| line.chars().count() <= 13));
    }

    #[test]
    fn renders_week_chart() {
        let chart = render_week_chart(
            &[1, 10, 4, 0, 2, 5, 3],
            &["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"],
        );
        assert!(chart.contains("Tue"));
        assert!(chart.contains("######"));
    }

    #[test]
    fn renders_cache_block_for_monospace() {
        let block = render_cache_block(&CommonsCacheStats {
            file_by_page_id_entries: 2,
            file_by_title_entries: 2,
            file_search_entries: 3,
            category_search_entries: 4,
            ram_file_bytes_entries: 1,
            ram_file_bytes_bytes: 10 * 1024 * 1024,
            ram_file_bytes_max_bytes: 100 * 1024 * 1024,
            tmp_file_bytes_entries: 2,
            tmp_file_bytes_bytes: 20 * 1024 * 1024,
            tmp_file_bytes_max_bytes: 200 * 1024 * 1024,
            ..CommonsCacheStats::default()
        });

        assert_eq!(
            block,
            "RAM meta       11\n\nRAM files\nused     10.0 MB\nlimit   100.0 MB\npct        10.0%\nentries        1\n\n/tmp files\nused     20.0 MB\nlimit   200.0 MB\npct        10.0%\nentries        2\n\nSearch          3\nCategory        4"
        );
        assert!(block.lines().all(|line| line.chars().count() <= 17));
    }

    #[test]
    fn parses_cloudwatch_metric_xml() {
        let xml = r#"
            <GetMetricStatisticsResponse>
              <GetMetricStatisticsResult>
                <Datapoints>
                  <member><Sum>3.0</Sum></member>
                  <member><Sum>7.0</Sum></member>
                </Datapoints>
              </GetMetricStatisticsResult>
            </GetMetricStatisticsResponse>
        "#;
        assert_eq!(parse_metric_values(xml, "Sum").unwrap(), vec![3.0, 7.0]);
    }

    #[test]
    fn parses_metric_xml_ignores_invalid_values_and_other_tags() {
        let xml = r#"
            <Response>
              <Average>12.5</Average>
              <Sum>bad</Sum>
              <Sum>7</Sum>
              <Maximum>99</Maximum>
            </Response>
        "#;

        assert_eq!(parse_metric_values(xml, "Sum").unwrap(), vec![7.0]);
        assert_eq!(parse_metric_values(xml, "Maximum").unwrap(), vec![99.0]);
    }

    #[test]
    fn renders_full_stats_dashboard_with_links_and_chart() {
        let config = Config {
            telegram_bot_token: None,
            telegram_webhook_secret: None,
            admin_user_ids: vec![42],
            github_url: "https://github.com/example/repo".into(),
            aws_region: "us-east-1".into(),
            lambda_function_name: "commons-bot".into(),
            dynamodb_table: Some("prefs".into()),
            stateless_mode: false,
            max_file_bytes: 50,
            user_agent: "test".into(),
            commons_api_url: "https://commons.wikimedia.org/w/api.php".into(),
            commons_auth_cookie_ssm_parameter: None,
            enable_test_endpoint: false,
        };
        let stats = StatsSnapshot {
            invocations_24h: 12,
            invocations_7d: 70,
            errors_24h: 1,
            errors_7d: 2,
            errors_month: 3,
            min_duration_ms: 10.0,
            avg_duration_ms: 1000.0,
            max_duration_ms: 2000.0,
            dynamodb_size_bytes: 1024 * 1024 * 1024,
            daily_invocations: [1, 2, 3, 4, 5, 6, 7],
            daily_labels: [
                "Mon".into(),
                "Tue".into(),
                "Wed".into(),
                "Thu".into(),
                "Fri".into(),
                "Sat".into(),
                "Sun".into(),
            ],
            commons_cache: CommonsCacheStats {
                ram_file_bytes_max_bytes: 1024,
                tmp_file_bytes_max_bytes: 2048,
                ..CommonsCacheStats::default()
            },
        };

        let rendered = stats.render_text(&config);

        assert!(rendered.contains("Stats"));
        assert!(rendered.contains("Calls per day"));
        assert!(rendered.contains("Lambda requests: 0.0%"));
        assert!(rendered.contains("DynamoDB storage: 4.000%"));
        assert!(rendered.contains("AWS Lambda free tier:\nhttps://aws.amazon.com/lambda/pricing/"));
        assert!(rendered.contains(
            "DynamoDB free tier: 25 GB, 25 RCU/WCU:\nhttps://aws.amazon.com/dynamodb/pricing/"
        ));
        assert!(rendered.contains(
            "CloudWatch:\nhttps://us-east-1.console.aws.amazon.com/cloudwatch/home?region=us-east-1#logsV2:log-groups/log-group/$252Faws$252Flambda$252Fcommons-bot"
        ));
        assert!(rendered.contains(
            "\nDynamoDB:\nhttps://us-east-1.console.aws.amazon.com/dynamodbv2/home?region=us-east-1#table?name=prefs"
        ));
    }

    #[test]
    fn renders_stateless_dashboard_without_dynamodb_link() {
        let config = Config {
            telegram_bot_token: None,
            telegram_webhook_secret: None,
            admin_user_ids: Vec::new(),
            github_url: String::new(),
            aws_region: "eu-west-1".into(),
            lambda_function_name: "function".into(),
            dynamodb_table: None,
            stateless_mode: true,
            max_file_bytes: 0,
            user_agent: String::new(),
            commons_api_url: String::new(),
            commons_auth_cookie_ssm_parameter: None,
            enable_test_endpoint: false,
        };

        let rendered = StatsSnapshot::default().render_text(&config);

        assert!(rendered.contains("DynamoDB is disabled in stateless mode"));
    }

    #[test]
    fn formats_bytes_and_percents() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MB");
        assert_eq!(percent(5.0, 0.0), 0.0);
        assert_eq!(percent(5.0, 10.0), 50.0);
    }

    #[test]
    fn weekday_labels_are_short_and_stable() {
        assert_eq!(weekday_label(Weekday::Monday), "Mon");
        assert_eq!(weekday_label(Weekday::Sunday), "Sun");
    }
}
