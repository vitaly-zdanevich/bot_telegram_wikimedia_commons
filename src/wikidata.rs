use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use tokio::sync::RwLock;

const WIKIDATA_API_URL: &str = "https://www.wikidata.org/w/api.php";
const WIKIDATA_ENTITY_BATCH_SIZE: usize = 50;
const WIKIDATA_PROPERTY_DISPLAY_LIMIT: usize = 30;
const WIKIDATA_VALUES_PER_PROPERTY_LIMIT: usize = 5;
const WIKIDATA_CATEGORY_MESSAGE_BUDGET_CHARS: usize = 2600;

static RENDERED_CLAIMS_CACHE: Lazy<RwLock<HashMap<String, String>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// HTTP client for Wikidata entity claims.
#[derive(Clone)]
pub struct WikidataClient {
    client: Client,
}

impl WikidataClient {
    /// Creates a Wikidata API client with the project user agent.
    pub fn new(user_agent: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .user_agent(user_agent.into())
                .build()
                .context("failed to build Wikidata HTTP client")?,
        })
    }

    /// Loads and renders clickable Wikidata key/value claims for a category item.
    pub async fn category_claims_html(&self, item: &str, language: &str) -> Result<String> {
        let language = normalize_language_code(language);
        let item = normalize_wikidata_entity_id(item).context("Wikidata item id is invalid")?;
        let cache_key = format!("{language}:{item}");
        if let Some(rendered) = RENDERED_CLAIMS_CACHE.read().await.get(&cache_key).cloned() {
            return Ok(rendered);
        }

        let claims = self.wikidata_claims(&language, &item).await?;
        let rendered = render_wikidata_claims_html(&claims, WIKIDATA_CATEGORY_MESSAGE_BUDGET_CHARS);
        RENDERED_CLAIMS_CACHE
            .write()
            .await
            .insert(cache_key, rendered.clone());
        Ok(rendered)
    }

    /// Loads Wikidata claims and resolves property/value labels.
    async fn wikidata_claims(&self, language: &str, item: &str) -> Result<WikidataClaims> {
        let entity = self
            .wikidata_entity(item, "labels|descriptions|claims", language)
            .await?;
        let claims_by_property = entity.claims.clone().unwrap_or_default();
        let mut label_ids = HashSet::from([item.to_string()]);
        for (property_id, claims) in &claims_by_property {
            label_ids.insert(property_id.clone());
            for claim in claims {
                if let Some(entity_id) = wikidata_entity_id_from_snak(&claim.mainsnak) {
                    label_ids.insert(entity_id);
                }
                if let Some(unit_id) = claim
                    .mainsnak
                    .datavalue
                    .as_ref()
                    .and_then(|datavalue| wikidata_quantity_unit_id(&datavalue.value))
                {
                    label_ids.insert(unit_id);
                }
            }
        }

        let external_id_property_ids = claims_by_property
            .iter()
            .filter(|(_property_id, claims)| {
                claims.iter().any(|claim| {
                    claim.mainsnak.datatype.as_deref() == Some("external-id")
                        && claim
                            .mainsnak
                            .datavalue
                            .as_ref()
                            .and_then(|datavalue| datavalue.value.as_str())
                            .is_some()
                })
            })
            .map(|(property_id, _claims)| property_id.clone())
            .collect::<Vec<_>>();

        let label_ids = label_ids.into_iter().collect::<Vec<_>>();
        let labels = self.wikidata_labels(language, &label_ids);
        let property_formatters = self.wikidata_property_formatters(&external_id_property_ids);
        let (labels, property_formatters) = tokio::join!(labels, property_formatters);
        let labels = labels?;
        let property_formatters = property_formatters?;

        let mut properties = claims_by_property
            .into_iter()
            .map(|(property_id, claims)| {
                let property_label = wikidata_label_text(&labels, &property_id)
                    .unwrap_or_else(|| property_id.clone());
                let values = claims
                    .iter()
                    .filter_map(|claim| {
                        render_wikidata_snak_value(
                            &property_id,
                            &claim.mainsnak,
                            &labels,
                            &property_formatters,
                        )
                    })
                    .take(WIKIDATA_VALUES_PER_PROPERTY_LIMIT)
                    .collect::<Vec<_>>();
                WikidataPropertyClaims {
                    property_id,
                    property_label,
                    total_values: claims.len(),
                    values,
                }
            })
            .collect::<Vec<_>>();
        properties.sort_by_key(|property| property.property_label.to_lowercase());

        Ok(WikidataClaims {
            item: item.to_string(),
            label: wikidata_label_text(&labels, item)
                .or_else(|| wikidata_localized_entity_value(entity.labels.as_ref(), language)),
            description: wikidata_description_text(&labels, item).or_else(|| {
                wikidata_localized_entity_value(entity.descriptions.as_ref(), language)
            }),
            property_count: properties.len(),
            properties,
        })
    }

    /// Loads a single Wikidata entity by id.
    async fn wikidata_entity(
        &self,
        item: &str,
        props: &str,
        language: &str,
    ) -> Result<WikidataEntity> {
        let response: WikidataEntitiesResponse = self
            .client
            .get(WIKIDATA_API_URL)
            .query(&[
                ("action", "wbgetentities"),
                ("format", "json"),
                ("ids", item),
                ("props", props),
                ("languages", &format!("{language}|en")),
                ("languagefallback", "1"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        response
            .entities
            .get(item)
            .cloned()
            .context("Wikidata response did not include requested entity")
    }

    /// Loads labels and descriptions for entity ids.
    async fn wikidata_labels(
        &self,
        language: &str,
        ids: &[String],
    ) -> Result<HashMap<String, WikidataLabelInfo>> {
        let mut unique_ids = ids
            .iter()
            .filter_map(|id| normalize_wikidata_entity_id(id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        unique_ids.sort();
        if unique_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut labels = HashMap::new();
        for chunk in unique_ids.chunks(WIKIDATA_ENTITY_BATCH_SIZE) {
            let response: WikidataEntitiesResponse = self
                .client
                .get(WIKIDATA_API_URL)
                .query(&[
                    ("action", "wbgetentities"),
                    ("format", "json"),
                    ("ids", &chunk.join("|")),
                    ("props", "labels|descriptions"),
                    ("languages", &format!("{language}|en")),
                    ("languagefallback", "1"),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            for (id, entity) in response.entities {
                labels.insert(
                    id,
                    WikidataLabelInfo {
                        label: wikidata_localized_entity_value(entity.labels.as_ref(), language),
                        description: wikidata_localized_entity_value(
                            entity.descriptions.as_ref(),
                            language,
                        ),
                    },
                );
            }
        }
        Ok(labels)
    }

    /// Loads external-id formatter URL patterns for properties.
    async fn wikidata_property_formatters(
        &self,
        property_ids: &[String],
    ) -> Result<HashMap<String, String>> {
        let mut unique_ids = property_ids
            .iter()
            .filter_map(|id| normalize_wikidata_entity_id(id))
            .filter(|id| id.starts_with('P'))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        unique_ids.sort();
        if unique_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut formatters = HashMap::new();
        for chunk in unique_ids.chunks(WIKIDATA_ENTITY_BATCH_SIZE) {
            let response: WikidataEntitiesResponse = self
                .client
                .get(WIKIDATA_API_URL)
                .query(&[
                    ("action", "wbgetentities"),
                    ("format", "json"),
                    ("ids", &chunk.join("|")),
                    ("props", "claims"),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            for property_id in chunk {
                if let Some(formatter) = response
                    .entities
                    .get(property_id)
                    .and_then(wikidata_formatter_url_from_entity)
                {
                    formatters.insert(property_id.clone(), formatter);
                }
            }
        }
        Ok(formatters)
    }
}

/// Renders clickable Wikidata claims within a Telegram-safe character budget.
fn render_wikidata_claims_html(claims: &WikidataClaims, limit: usize) -> String {
    let mut lines = Vec::new();
    let title = claims
        .label
        .as_ref()
        .map(|label| {
            if label == &claims.item {
                claims.item.clone()
            } else {
                format!("{label} ({})", claims.item)
            }
        })
        .unwrap_or_else(|| claims.item.clone());
    lines.push(html_bold_link(&wikidata_entity_url(&claims.item), &title));
    if let Some(description) = claims.description.as_deref() {
        lines.push(html_escape_text(description));
    }
    lines.push(format!("Properties: {}", claims.property_count));

    let mut rendered_properties = 0_usize;
    for property in claims
        .properties
        .iter()
        .take(WIKIDATA_PROPERTY_DISPLAY_LIMIT)
    {
        let property_title = if property.property_label == property.property_id {
            property.property_id.clone()
        } else {
            format!("{} ({})", property.property_label, property.property_id)
        };
        let property_link = html_link(&wikidata_entity_url(&property.property_id), &property_title);
        let mut values = if property.values.is_empty() {
            vec![html_escape_text("no value")]
        } else {
            property.values.clone()
        };
        if property.total_values > values.len() {
            values.push(format!("+{} more", property.total_values - values.len()));
        }
        let next_line = format!("{property_link}: {}", values.join(", "));
        if lines.join("\n").chars().count() + next_line.chars().count() + 1 > limit {
            break;
        }
        rendered_properties += 1;
        lines.push(next_line);
    }

    if rendered_properties < claims.properties.len() {
        lines.push(format!(
            "Shown first {rendered_properties} of {} properties.",
            claims.properties.len()
        ));
    }

    lines.join("\n")
}

fn render_wikidata_snak_value(
    property_id: &str,
    snak: &WikidataSnak,
    labels: &HashMap<String, WikidataLabelInfo>,
    property_formatters: &HashMap<String, String>,
) -> Option<String> {
    let Some(datavalue) = &snak.datavalue else {
        return match snak.snaktype.as_deref() {
            Some("somevalue") => Some(html_escape_text("some value")),
            Some("novalue") => Some(html_escape_text("no value")),
            _ => None,
        };
    };

    if let Some(entity_id) = wikidata_entity_id_from_value(&datavalue.value) {
        return Some(wikidata_entity_link(&entity_id, labels));
    }

    if let Some(value) = datavalue.value.as_str() {
        if snak.datatype.as_deref() == Some("external-id")
            && let Some(formatter) = property_formatters.get(property_id)
            && let Some(url) = wikidata_external_id_url(formatter, value)
        {
            return Some(html_link(&url, value));
        }
        if snak.datatype.as_deref() == Some("commonsMedia") {
            return Some(html_link(&commons_file_url(value), value));
        }
        return if is_absolute_url(value) {
            Some(html_link(value, value))
        } else {
            Some(html_escape_text(value))
        };
    }

    if let Some(value) = datavalue.value.as_object() {
        if let Some(rendered) = wikidata_coordinate_value(value) {
            return Some(rendered);
        }
        if let Some(rendered) = wikidata_time_value(value) {
            return Some(rendered);
        }
        if let Some(rendered) = wikidata_quantity_value(value, labels) {
            return Some(rendered);
        }
        if let Some(rendered) = wikidata_monolingual_text_value(value) {
            return Some(rendered);
        }
    }

    Some(html_escape_text(&truncate_for_telegram(
        &datavalue.value.to_string(),
        240,
    )))
}

fn wikidata_coordinate_value(value: &serde_json::Map<String, Value>) -> Option<String> {
    let lat = value.get("latitude")?.as_f64()?;
    let lon = value.get("longitude")?.as_f64()?;
    Some(html_link(
        &coordinate_url(lat, lon),
        &format!("{lat}, {lon}"),
    ))
}

fn wikidata_time_value(value: &serde_json::Map<String, Value>) -> Option<String> {
    let raw = value.get("time")?.as_str()?;
    let mut formatted = raw.trim_start_matches('+').to_string();
    if let Some((date, _time)) = formatted.split_once('T') {
        formatted = date.to_string();
    }
    while formatted.ends_with("-00") {
        formatted.truncate(formatted.len().saturating_sub(3));
    }
    (!formatted.is_empty()).then(|| html_escape_text(&formatted))
}

fn wikidata_quantity_value(
    value: &serde_json::Map<String, Value>,
    labels: &HashMap<String, WikidataLabelInfo>,
) -> Option<String> {
    let amount = value.get("amount")?.as_str()?.trim_start_matches('+');
    let Some(unit) = value.get("unit").and_then(Value::as_str) else {
        return Some(html_escape_text(amount));
    };
    if unit == "1" {
        return Some(html_escape_text(amount));
    }

    let unit = wikidata_entity_id_from_url(unit)
        .map(|id| wikidata_entity_link(&id, labels))
        .unwrap_or_else(|| html_link(unit, unit));
    Some(format!("{} {}", html_escape_text(amount), unit))
}

fn wikidata_monolingual_text_value(value: &serde_json::Map<String, Value>) -> Option<String> {
    let text = value.get("text")?.as_str()?;
    let language = value.get("language").and_then(Value::as_str);
    Some(match language {
        Some(language) => format!(
            "{} ({})",
            html_escape_text(text),
            html_escape_text(language)
        ),
        None => html_escape_text(text),
    })
}

fn wikidata_external_id_url(formatter: &str, value: &str) -> Option<String> {
    let formatter = formatter.trim();
    let value = value.trim();
    if formatter.is_empty() || value.is_empty() || !formatter.contains("$1") {
        return None;
    }

    let url = formatter.replace("$1", &urlencoding::encode(value));
    is_absolute_url(&url).then_some(url)
}

fn wikidata_entity_link(entity_id: &str, labels: &HashMap<String, WikidataLabelInfo>) -> String {
    let text = wikidata_label_text(labels, entity_id)
        .filter(|label| label != entity_id)
        .map(|label| format!("{label} ({entity_id})"))
        .unwrap_or_else(|| entity_id.to_string());
    html_link(&wikidata_entity_url(entity_id), &text)
}

fn wikidata_label_text(
    labels: &HashMap<String, WikidataLabelInfo>,
    entity_id: &str,
) -> Option<String> {
    labels.get(entity_id).and_then(|label| label.label.clone())
}

fn wikidata_description_text(
    labels: &HashMap<String, WikidataLabelInfo>,
    entity_id: &str,
) -> Option<String> {
    labels
        .get(entity_id)
        .and_then(|label| label.description.clone())
}

fn wikidata_entity_id_from_snak(snak: &WikidataSnak) -> Option<String> {
    snak.datavalue
        .as_ref()
        .and_then(|datavalue| wikidata_entity_id_from_value(&datavalue.value))
}

fn wikidata_entity_id_from_value(value: &Value) -> Option<String> {
    if let Some(id) = value
        .get("id")
        .and_then(Value::as_str)
        .and_then(normalize_wikidata_entity_id)
    {
        return Some(id);
    }

    let numeric_id = value.get("numeric-id")?.as_u64()?;
    match value.get("entity-type").and_then(Value::as_str) {
        Some("item") => Some(format!("Q{numeric_id}")),
        Some("property") => Some(format!("P{numeric_id}")),
        Some("lexeme") => Some(format!("L{numeric_id}")),
        _ => None,
    }
}

fn wikidata_quantity_unit_id(value: &Value) -> Option<String> {
    value
        .get("unit")
        .and_then(Value::as_str)
        .and_then(wikidata_entity_id_from_url)
}

fn wikidata_entity_id_from_url(url: &str) -> Option<String> {
    url.rsplit('/')
        .next()
        .and_then(normalize_wikidata_entity_id)
}

fn wikidata_localized_entity_value(
    values: Option<&HashMap<String, WikidataLocalizedValue>>,
    language: &str,
) -> Option<String> {
    let values = values?;
    values
        .get(language)
        .or_else(|| values.get("en"))
        .or_else(|| values.values().next())
        .map(|value| value.value.clone())
}

fn normalize_wikidata_entity_id(value: &str) -> Option<String> {
    let value = value.trim();
    let mut chars = value.chars();
    let prefix = chars.next()?.to_ascii_uppercase();
    if !matches!(prefix, 'Q' | 'P' | 'L') {
        return None;
    }
    let rest = chars.collect::<String>();
    (!rest.is_empty() && rest.chars().all(|character| character.is_ascii_digit()))
        .then(|| format!("{prefix}{rest}"))
}

fn normalize_language_code(value: &str) -> String {
    let lower = value.trim().to_ascii_lowercase();
    if lower
        .chars()
        .all(|character| character.is_ascii_lowercase() || character == '-')
        && !lower.is_empty()
    {
        lower
    } else {
        "en".to_string()
    }
}

fn wikidata_entity_url(entity_id: &str) -> String {
    if entity_id.starts_with('P') {
        format!("https://www.wikidata.org/wiki/Property:{entity_id}")
    } else {
        format!("https://www.wikidata.org/wiki/{entity_id}")
    }
}

fn commons_file_url(file: &str) -> String {
    let file = file.trim();
    let title = if file.to_ascii_lowercase().starts_with("file:") {
        file.to_string()
    } else {
        format!("File:{file}")
    };
    format!(
        "https://commons.wikimedia.org/wiki/{}",
        urlencoding::encode(&title.replace(' ', "_"))
    )
}

fn coordinate_url(lat: f64, lon: f64) -> String {
    format!("https://www.openstreetmap.org/?mlat={lat}&mlon={lon}#map=15/{lat}/{lon}")
}

fn is_absolute_url(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
}

fn html_bold_link(url: &str, text: &str) -> String {
    format!("<b>{}</b>", html_link(url, text))
}

fn html_link(url: &str, text: &str) -> String {
    format!(
        "<a href=\"{}\">{}</a>",
        html_escape_attr(url),
        html_escape_text(text)
    )
}

fn html_escape_text(value: &str) -> String {
    html_escape::encode_text(value).to_string()
}

fn html_escape_attr(value: &str) -> String {
    html_escape::encode_double_quoted_attribute(value).to_string()
}

fn truncate_for_telegram(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let mut truncated = value.chars().take(keep).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn wikidata_formatter_url_from_entity(entity: &WikidataEntity) -> Option<String> {
    entity
        .claims
        .as_ref()?
        .get("P1630")?
        .iter()
        .find_map(|claim| {
            claim
                .mainsnak
                .datavalue
                .as_ref()?
                .value
                .as_str()
                .map(str::to_string)
                .filter(|formatter| is_absolute_url(formatter) && formatter.contains("$1"))
        })
}

#[derive(Debug, Clone)]
struct WikidataClaims {
    item: String,
    label: Option<String>,
    description: Option<String>,
    property_count: usize,
    properties: Vec<WikidataPropertyClaims>,
}

#[derive(Debug, Clone)]
struct WikidataPropertyClaims {
    property_id: String,
    property_label: String,
    total_values: usize,
    values: Vec<String>,
}

#[derive(Debug, Clone)]
struct WikidataLabelInfo {
    label: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WikidataEntitiesResponse {
    entities: HashMap<String, WikidataEntity>,
}

#[derive(Debug, Clone, Deserialize)]
struct WikidataEntity {
    claims: Option<HashMap<String, Vec<WikidataClaim>>>,
    labels: Option<HashMap<String, WikidataLocalizedValue>>,
    descriptions: Option<HashMap<String, WikidataLocalizedValue>>,
}

#[derive(Debug, Clone, Deserialize)]
struct WikidataClaim {
    mainsnak: WikidataSnak,
}

#[derive(Debug, Clone, Deserialize)]
struct WikidataSnak {
    datatype: Option<String>,
    snaktype: Option<String>,
    datavalue: Option<WikidataDataValue>,
}

#[derive(Debug, Clone, Deserialize)]
struct WikidataDataValue {
    value: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct WikidataLocalizedValue {
    value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_clickable_claim_keys_and_values() {
        let claims = WikidataClaims {
            item: "Q686963".to_string(),
            label: Some("Armies of Exigo".to_string()),
            description: Some("real-time strategy video game".to_string()),
            property_count: 2,
            properties: vec![
                WikidataPropertyClaims {
                    property_id: "P31".to_string(),
                    property_label: "instance of".to_string(),
                    total_values: 1,
                    values: vec![html_link(
                        "https://www.wikidata.org/wiki/Q7889",
                        "video game (Q7889)",
                    )],
                },
                WikidataPropertyClaims {
                    property_id: "P577".to_string(),
                    property_label: "publication date".to_string(),
                    total_values: 1,
                    values: vec![html_escape_text("2004-11-30")],
                },
            ],
        };

        let rendered = render_wikidata_claims_html(&claims, 3900);

        assert!(rendered.contains(
            "<b><a href=\"https://www.wikidata.org/wiki/Q686963\">Armies of Exigo (Q686963)</a></b>"
        ));
        assert!(rendered.contains(
            "<a href=\"https://www.wikidata.org/wiki/Property:P31\">instance of (P31)</a>: <a href=\"https://www.wikidata.org/wiki/Q7889\">video game (Q7889)</a>"
        ));
        assert!(rendered.contains("Properties: 2"));
    }

    #[test]
    fn wikidata_property_entity_urls_use_property_namespace() {
        assert_eq!(
            wikidata_entity_url("P12245"),
            "https://www.wikidata.org/wiki/Property:P12245"
        );
        assert_eq!(
            wikidata_entity_url("Q686963"),
            "https://www.wikidata.org/wiki/Q686963"
        );
    }

    #[test]
    fn url_values_render_clickable() {
        let rendered = render_wikidata_snak_value(
            "P856",
            &WikidataSnak {
                datatype: Some("url".to_string()),
                snaktype: Some("value".to_string()),
                datavalue: Some(WikidataDataValue {
                    value: Value::String("https://example.com/source".to_string()),
                }),
            },
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(
            rendered,
            "<a href=\"https://example.com/source\">https://example.com/source</a>"
        );
    }

    #[test]
    fn external_id_values_render_clickable_with_formatter() {
        let rendered = render_wikidata_snak_value(
            "P12245",
            &WikidataSnak {
                datatype: Some("external-id".to_string()),
                snaktype: Some("value".to_string()),
                datavalue: Some(WikidataDataValue {
                    value: Value::String("2888".to_string()),
                }),
            },
            &HashMap::new(),
            &HashMap::from([(
                "P12245".to_string(),
                "https://www.gamesmeter.nl/game/$1".to_string(),
            )]),
        )
        .unwrap();

        assert_eq!(
            rendered,
            "<a href=\"https://www.gamesmeter.nl/game/2888\">2888</a>"
        );
    }

    #[test]
    fn external_id_formatter_urls_are_url_encoded() {
        assert_eq!(
            wikidata_external_id_url("https://example.com/search?id=$1", "a b/1"),
            Some("https://example.com/search?id=a%20b%2F1".to_string())
        );
    }
}
