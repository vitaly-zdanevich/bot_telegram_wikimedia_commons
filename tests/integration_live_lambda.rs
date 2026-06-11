//! Opt-in live Lambda integration tests.
//!
//! Set `LIVE_LAMBDA_URL` and `TELEGRAM_WEBHOOK_SECRET` to enable. The default
//! test run skips these tests, and the live path does not send Telegram messages.

#[tokio::test]
async fn live_lambda_parses_query_when_configured() {
    let Ok(url) = std::env::var("LIVE_LAMBDA_URL") else {
        return;
    };
    let secret = std::env::var("TELEGRAM_WEBHOOK_SECRET").unwrap_or_default();
    let client = reqwest::Client::new();
    assert_live_intent(&client, &url, &secret, "flac Minsk c music", "FileSearch").await;
    assert_live_intent(&client, &url, &secret, "cat minsk", "FileSearch").await;
    assert_live_intent(&client, &url, &secret, "c minsk", "CategorySearch").await;
    assert_live_intent(&client, &url, &secret, "c:minsk", "CategorySearch").await;
}

/// Calls the live parser endpoint and verifies the expected intent type.
async fn assert_live_intent(
    client: &reqwest::Client,
    url: &str,
    secret: &str,
    query: &str,
    expected_intent: &str,
) {
    let endpoint = format!(
        "{}/__test?q={}",
        url.trim_end_matches('/'),
        urlencoding::encode(query)
    );
    let response = client
        .get(endpoint)
        .header("x-telegram-bot-api-secret-token", secret)
        .send()
        .await
        .expect("live Lambda request should complete");
    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.expect("test endpoint returns JSON");
    assert_eq!(body["ok"], true);
    assert!(
        body["intent"]
            .as_str()
            .unwrap_or_default()
            .contains(expected_intent)
    );
}
