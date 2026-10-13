#!/bin/bash
# Create (`up`) or delete (`down`) the AWS resources the probe functions use:
# an IAM role, a DynamoDB table with one seeded item, a KMS key and an S3
# bucket. `role` creates only the role, enough for the first-touch probe, which
# makes no authenticated calls. Every name carries the `oneoff-sdk-tuning` prefix, outside bencher's
# `lambdabench-` names, so neither side touches the other's resources. `up`
# writes the ids the functions need to .tmp/oneoff-rust-sdk-tuning/resources-<region>.env.
# AWS_REGION picks the region (default eu-central-1). Regions can be up at the same
# time: the role is global and gets one data policy per region.
set -euo pipefail
REGION="${AWS_REGION:-eu-central-1}"
export AWS_REGION="$REGION"
PREFIX="oneoff-sdk-tuning"
ROLE="$PREFIX-role"
TABLE="$PREFIX-table"
ALIAS="alias/$PREFIX-key"
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$REPO/.tmp/oneoff-rust-sdk-tuning"
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
BUCKET="$PREFIX-$ACCOUNT-$REGION"
POLICY="$PREFIX-data-$REGION"
ENV_FILE="$OUT/resources-$REGION.env"
mkdir -p "$OUT"

create_role() {
  aws iam get-role --role-name "$ROLE" >/dev/null 2>&1 || aws iam create-role --role-name "$ROLE" \
    --assume-role-policy-document '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}' >/dev/null
  aws iam attach-role-policy --role-name "$ROLE" \
    --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
}

role() {
  create_role
  # Keep the ids an earlier `up` wrote for this region: overwriting them with
  # placeholders would break the SDK probes that use the table, key and bucket.
  [ -f "$ENV_FILE" ] && return 0
  # probe.py expects every key; the first-touch probe reads none of the PROBE_ ones.
  cat >"$ENV_FILE" <<ENV
ROLE_ARN=arn:aws:iam::$ACCOUNT:role/$ROLE
PROBE_TABLE=unused
PROBE_KEY=unused
PROBE_KMS_KEY_ID=unused
PROBE_BUCKET=unused
PROBE_ORDER_PK=unused
PROBE_RECEIPT_KEY=unused
ENV
}

up() {
  create_role
  if ! aws dynamodb describe-table --table-name "$TABLE" >/dev/null 2>&1; then
    aws dynamodb create-table --table-name "$TABLE" --billing-mode PAY_PER_REQUEST \
      --attribute-definitions AttributeName=pk,AttributeType=S \
      --key-schema AttributeName=pk,KeyType=HASH >/dev/null
    aws dynamodb wait table-exists --table-name "$TABLE"
  fi
  aws dynamodb put-item --table-name "$TABLE" --item '{"pk":{"S":"seed"},"payload":{"S":"hello"}}'
  key_id=$(aws kms describe-key --key-id "$ALIAS" --query KeyMetadata.KeyId --output text 2>/dev/null || true)
  if [ -z "$key_id" ]; then
    key_id=$(aws kms create-key --description "$PREFIX probe" --query KeyMetadata.KeyId --output text)
    aws kms create-alias --alias-name "$ALIAS" --target-key-id "$key_id"
  fi
  if ! aws s3api head-bucket --bucket "$BUCKET" 2>/dev/null; then
    # us-east-1 is the default location and rejects an explicit constraint.
    if [ "$REGION" = us-east-1 ]; then
      aws s3api create-bucket --bucket "$BUCKET" >/dev/null
    else
      aws s3api create-bucket --bucket "$BUCKET" \
        --create-bucket-configuration "LocationConstraint=$REGION" >/dev/null
    fi
  fi
  aws iam put-role-policy --role-name "$ROLE" --policy-name "$POLICY" --policy-document "{
    \"Version\":\"2012-10-17\",\"Statement\":[
    {\"Effect\":\"Allow\",\"Action\":[\"dynamodb:GetItem\",\"dynamodb:PutItem\"],\"Resource\":\"arn:aws:dynamodb:$REGION:$ACCOUNT:table/$TABLE\"},
    {\"Effect\":\"Allow\",\"Action\":\"kms:Encrypt\",\"Resource\":\"arn:aws:kms:$REGION:$ACCOUNT:key/$key_id\"},
    {\"Effect\":\"Allow\",\"Action\":[\"s3:GetObject\",\"s3:PutObject\"],\"Resource\":\"arn:aws:s3:::$BUCKET/*\"}]}"
  cat >"$ENV_FILE" <<ENV
ROLE_ARN=arn:aws:iam::$ACCOUNT:role/$ROLE
PROBE_TABLE=$TABLE
PROBE_KEY=seed
PROBE_KMS_KEY_ID=$key_id
PROBE_BUCKET=$BUCKET
PROBE_ORDER_PK=order-probe
PROBE_RECEIPT_KEY=receipts/probe.txt
ENV
  cat "$ENV_FILE"
}

down() {
  aws iam delete-role-policy --role-name "$ROLE" --policy-name "$POLICY" 2>/dev/null || true
  # Keep the role while another region's data policy still uses it.
  others=$(aws iam list-role-policies --role-name "$ROLE" --query 'length(PolicyNames)' --output text 2>/dev/null || echo 0)
  if [ "$others" = 0 ]; then
    aws iam detach-role-policy --role-name "$ROLE" \
      --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole 2>/dev/null || true
    aws iam delete-role --role-name "$ROLE" 2>/dev/null || true
  fi
  aws dynamodb delete-table --table-name "$TABLE" >/dev/null 2>&1 || true
  aws s3 rb "s3://$BUCKET" --force >/dev/null 2>&1 || true
  key_id=$(aws kms describe-key --key-id "$ALIAS" --query KeyMetadata.KeyId --output text 2>/dev/null || true)
  if [ -n "$key_id" ]; then
    aws kms delete-alias --alias-name "$ALIAS"
    # 7 days is KMS's minimum waiting period; the key is unusable meanwhile.
    aws kms schedule-key-deletion --key-id "$key_id" --pending-window-in-days 7 >/dev/null
  fi
  rm -f "$ENV_FILE"
}

"${1:?usage: resources.sh up|role|down}"
