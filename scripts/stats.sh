#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FUNCTION_NAME="${FUNCTION_NAME:-$(terraform -chdir="$ROOT_DIR/infra" output -raw function_name 2>/dev/null || echo telegram-wikimedia-commons-bot)}"
DYNAMODB_TABLE="${DYNAMODB_TABLE:-$(terraform -chdir="$ROOT_DIR/infra" output -raw dynamodb_table 2>/dev/null || echo telegram-wikimedia-commons-bot-preferences)}"
FUNCTION_URL="$(terraform -chdir="$ROOT_DIR/infra" output -raw function_url 2>/dev/null || true)"
if [[ -n "$FUNCTION_URL" ]]; then
  URL_REGION="$(printf '%s' "$FUNCTION_URL" | sed -n 's#.*lambda-url\.\([a-z0-9-]*\)\.on\.aws.*#\1#p')"
fi
AWS_REGION="${URL_REGION:-${AWS_REGION:-${AWS_DEFAULT_REGION:-us-east-1}}}"
MEMORY_MB="${LAMBDA_MEMORY_MB:-3008}"

START_24H="$(date -u -d '24 hours ago' +%FT%TZ)"
START_7D="$(date -u -d '7 days ago' +%FT%TZ)"
END="$(date -u +%FT%TZ)"

metric_sum() {
  local metric="$1" start="$2" period="$3" stat="${4:-Sum}"
  aws cloudwatch get-metric-statistics \
    --region "$AWS_REGION" \
    --namespace AWS/Lambda \
    --metric-name "$metric" \
    --dimensions "Name=FunctionName,Value=$FUNCTION_NAME" \
    --start-time "$start" \
    --end-time "$END" \
    --period "$period" \
    --statistics "$stat" \
    --output json \
    | jq "([.Datapoints[].$stat] | add // 0) | floor"
}

duration_stat() {
  local stat="$1"
  aws cloudwatch get-metric-statistics \
    --region "$AWS_REGION" \
    --namespace AWS/Lambda \
    --metric-name Duration \
    --dimensions "Name=FunctionName,Value=$FUNCTION_NAME" \
    --start-time "$START_7D" \
    --end-time "$END" \
    --period 86400 \
    --statistics "$stat" \
    --output json \
    | jq "[.Datapoints[].$stat] | if length == 0 then 0 else add / length end"
}

CALLS_24H="$(metric_sum Invocations "$START_24H" 3600)"
CALLS_7D="$(metric_sum Invocations "$START_7D" 86400)"
ERRORS_24H="$(metric_sum Errors "$START_24H" 3600)"
ERRORS_7D="$(metric_sum Errors "$START_7D" 86400)"
MIN_MS="$(duration_stat Minimum)"
AVG_MS="$(duration_stat Average)"
MAX_MS="$(duration_stat Maximum)"
DDB_BYTES="$(aws dynamodb describe-table --region "$AWS_REGION" --table-name "$DYNAMODB_TABLE" --query 'Table.TableSizeBytes' --output text 2>/dev/null || echo 0)"

echo "Wikimedia Commons bot stats"
echo "Region: $AWS_REGION"
echo "Lambda: $FUNCTION_NAME"
echo
echo "Calls: 24h=$CALLS_24H  7d=$CALLS_7D"
echo "Errors: 24h=$ERRORS_24H  7d=$ERRORS_7D"
printf 'Duration: min=%.0fms avg=%.0fms max=%.0fms\n' "$MIN_MS" "$AVG_MS" "$MAX_MS"
printf 'DynamoDB table size: %.6f GB\n' "$(awk "BEGIN { print $DDB_BYTES / 1024 / 1024 / 1024 }")"
echo

echo "Calls per day"
for i in 6 5 4 3 2 1 0; do
  day_start="$(date -u -d "$i days ago 00:00" +%FT%TZ)"
  day_end="$(date -u -d "$i days ago 23:59" +%FT%TZ)"
  label="$(date -u -d "$i days ago" +%a)"
  count="$(aws cloudwatch get-metric-statistics \
    --region "$AWS_REGION" \
    --namespace AWS/Lambda \
    --metric-name Invocations \
    --dimensions "Name=FunctionName,Value=$FUNCTION_NAME" \
    --start-time "$day_start" \
    --end-time "$day_end" \
    --period 86400 \
    --statistics Sum \
    --output json | jq '([.Datapoints[].Sum] | add // 0) | floor')"
  DAY_COUNTS+=("$count")
  DAY_LABELS+=("$label")
done
max=1
for count in "${DAY_COUNTS[@]}"; do
  if awk "BEGIN { exit !($count > $max) }"; then
    max="$count"
  fi
done
for idx in "${!DAY_COUNTS[@]}"; do
  count="${DAY_COUNTS[$idx]}"
  label="${DAY_LABELS[$idx]}"
  width="$(awk "BEGIN { printf \"%d\", (($count / $max) * 20) + 0.5 }")"
  if [[ "$width" -lt 1 ]]; then
    width=1
  fi
  printf '%s %8s %s\n' "$label" "$count" "$(printf '#%.0s' $(seq 1 "$width"))"
done

GB_SECONDS="$(awk "BEGIN { print $CALLS_7D * ($AVG_MS / 1000) * ($MEMORY_MB / 1024) }")"
REQ_PCT="$(awk "BEGIN { print ($CALLS_7D / 1000000) * 100 }")"
DUR_PCT="$(awk "BEGIN { print ($GB_SECONDS / 400000) * 100 }")"
DDB_PCT="$(awk "BEGIN { print (($DDB_BYTES / 1024 / 1024 / 1024) / 25) * 100 }")"
echo
printf 'Free tier estimate: Lambda requests %.2f%%, Lambda duration %.2f%%, DynamoDB storage %.6f%%\n' "$REQ_PCT" "$DUR_PCT" "$DDB_PCT"
echo "AWS Lambda free tier: https://aws.amazon.com/lambda/pricing/"
echo "DynamoDB free tier: https://aws.amazon.com/dynamodb/pricing/"
echo "CloudWatch: https://${AWS_REGION}.console.aws.amazon.com/cloudwatch/home?region=${AWS_REGION}#logsV2:log-groups/log-group/\$252Faws\$252Flambda\$252F${FUNCTION_NAME}"
echo "DynamoDB: https://${AWS_REGION}.console.aws.amazon.com/dynamodbv2/home?region=${AWS_REGION}#table?name=${DYNAMODB_TABLE}"
