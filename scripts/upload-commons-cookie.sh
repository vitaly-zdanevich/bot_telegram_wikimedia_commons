#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COOKIE_FILE="${1:-/home/vitaly/.pywikibot/pywikibot-Vitaly_Zdanevich.lwp}"
PARAMETER_NAME="${2:-/telegram-wikimedia-commons-bot/commons-auth-cookie-jar}"
REGION="${AWS_REGION:-${AWS_DEFAULT_REGION:-us-east-1}}"

if [[ ! -r "$COOKIE_FILE" ]]; then
  echo "Cookie file is not readable: $COOKIE_FILE" >&2
  exit 1
fi

if [[ "$PARAMETER_NAME" != /* ]]; then
  echo "SSM parameter name must start with /: $PARAMETER_NAME" >&2
  exit 1
fi

COOKIE_VALUE="$(
  awk '
    /^Set-Cookie3: / && /wikimedia\.org/ {
      line = $0
      sub(/^Set-Cookie3: /, "", line)
      split(line, attrs, ";")
      pair = attrs[1]
      eq = index(pair, "=")
      if (eq == 0) {
        next
      }
      name = substr(pair, 1, eq - 1)
      value = substr(pair, eq + 1)
      gsub(/^"/, "", value)
      gsub(/"$/, "", value)
      gsub(/\\"/, "\"", value)
      gsub(/\\\\/, "\\", value)
      printf "%s%s=%s", sep, name, value
      sep = "; "
    }
    END {
      if (sep != "") {
        printf "\n"
      }
    }
  ' "$COOKIE_FILE"
)"

if [[ -z "$COOKIE_VALUE" ]]; then
  echo "No Wikimedia cookies found in $COOKIE_FILE" >&2
  exit 1
fi

if (( ${#COOKIE_VALUE} > 4096 )); then
  echo "Compact cookie header is ${#COOKIE_VALUE} bytes, above the 4096-byte SSM Standard limit." >&2
  echo "Refusing to create a paid Advanced parameter." >&2
  exit 1
fi

aws ssm put-parameter \
  --region "$REGION" \
  --name "$PARAMETER_NAME" \
  --type SecureString \
  --overwrite \
  --value "$COOKIE_VALUE" \
  >/dev/null

echo "Uploaded Commons cookie jar to SSM parameter $PARAMETER_NAME in $REGION"
echo "Set commons_auth_cookie_ssm_parameter = \"$PARAMETER_NAME\" in infra/terraform.tfvars"
