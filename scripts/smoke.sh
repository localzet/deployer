#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${1:-http://localhost:8080}"
API_TOKEN="${API_TOKEN:-}"

if [[ -z "${API_TOKEN}" ]]; then
  echo "API_TOKEN is required" >&2
  exit 1
fi

echo "== healthz =="
curl -fsS "${BASE_URL}/healthz"
echo

echo "== dashboard auth =="
curl -fsS \
  -H "Authorization: Bearer ${API_TOKEN}" \
  "${BASE_URL}/api/dashboard" \
  | head -c 500
echo
echo

echo "== deployments auth =="
curl -fsS \
  -H "Authorization: Bearer ${API_TOKEN}" \
  "${BASE_URL}/api/deployments" \
  | head -c 500
echo

echo "Smoke check passed"
