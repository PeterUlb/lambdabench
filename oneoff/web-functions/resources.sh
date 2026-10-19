#!/bin/bash
# Create (`up`) or delete (`down`) the shared resources: an IAM role, a versioned S3 bucket
# (Web Function code + client results), a DynamoDB table with the seeded item `oneclient` reads,
# and the in-region client function. `down` also deletes every `oneoff-webfn-*` Web Function,
# Event Function and log group. Lambda can recreate a Web Function's log group from shutdown
# events a few minutes after deletion, so run `down` a second time ~7 min later.
source "$(dirname "$0")/common.sh"

up() {
  aws iam get-role --role-name "$ROLE" >/dev/null 2>&1 || aws iam create-role --role-name "$ROLE" \
    --assume-role-policy-document '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}' >/dev/null
  aws iam put-role-policy --role-name "$ROLE" --policy-name "$PREFIX-$REGION" --policy-document "$(cat <<J
{"Version":"2012-10-17","Statement":[
 {"Effect":"Allow","Action":["logs:CreateLogGroup","logs:CreateLogStream","logs:PutLogEvents"],"Resource":"*"},
 {"Effect":"Allow","Action":"dynamodb:GetItem","Resource":"arn:aws:dynamodb:$REGION:$ACCOUNT:table/$TABLE"},
 {"Effect":"Allow","Action":["lambda:InvokeFunction","lambda:GetFunction","lambda:UpdateFunctionConfiguration"],"Resource":"arn:aws:lambda:$REGION:$ACCOUNT:function:$PREFIX-*"},
 {"Effect":"Allow","Action":"lambda:InvokeWebFunctionEndpoint","Resource":"arn:aws:lambda:$REGION:$ACCOUNT:web-function/$PREFIX-*"},
 {"Effect":"Allow","Action":"s3:PutObject","Resource":"arn:aws:s3:::$BUCKET/results/*"}]}
J
)"
  if ! aws s3api head-bucket --bucket "$BUCKET" >/dev/null 2>&1; then
    aws s3api create-bucket --bucket "$BUCKET" --create-bucket-configuration LocationConstraint="$REGION" >/dev/null
  fi
  aws s3api put-bucket-versioning --bucket "$BUCKET" --versioning-configuration Status=Enabled
  aws s3api put-bucket-policy --bucket "$BUCKET" --policy "$(cat <<J
{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},
 "Action":["s3:GetObject","s3:GetObjectVersion"],"Resource":"arn:aws:s3:::$BUCKET/code/*",
 "Condition":{"ArnLike":{"aws:SourceArn":"arn:aws:lambda:$REGION:$ACCOUNT:web-function/$PREFIX-*/revision/*"}}}]}
J
)"
  if ! aws dynamodb describe-table --table-name "$TABLE" >/dev/null 2>&1; then
    aws dynamodb create-table --table-name "$TABLE" --attribute-definitions AttributeName=pk,AttributeType=S \
      --key-schema AttributeName=pk,KeyType=HASH --billing-mode PAY_PER_REQUEST >/dev/null
    aws dynamodb wait table-exists --table-name "$TABLE"
  fi
  aws dynamodb put-item --table-name "$TABLE" --item '{"pk":{"S":"seed"},"payload":{"S":"lambdabench-seeded-payload-value"}}'

  (cd "$HERE/client" && npm install --silent && npm run -s build >/dev/null)
  rm -f "$OUT/build/client.zip" && (cd "$HERE/client/dist" && zip -qr "$OUT/build/client.zip" .)
  if aws lambda get-function --function-name "$CLIENT" >/dev/null 2>&1; then
    aws lambda update-function-code --function-name "$CLIENT" --zip-file "fileb://$OUT/build/client.zip" >/dev/null
  else
    sleep 10 # a freshly created role takes a few seconds to become assumable
    aws lambda create-function --function-name "$CLIENT" --runtime nodejs24.x --architectures arm64 \
      --memory-size 2048 --timeout 900 --role "$ROLE_ARN" --handler index.handler \
      --zip-file "fileb://$OUT/build/client.zip" --environment "Variables={RESULT_BUCKET=$BUCKET}" >/dev/null
  fi
  aws lambda wait function-updated-v2 --function-name "$CLIENT"
  echo "up: role $ROLE, bucket $BUCKET, table $TABLE, client $CLIENT in $REGION"
}

down() {
  for f in $(aws lambda-web list-web-functions --query "functions[?starts_with(functionName, '$PREFIX-')].functionName" --output text); do
    echo "delete Web Function $f"; aws lambda-web delete-web-function --function-name "$f" >/dev/null
  done
  for f in $(aws lambda list-functions --query "Functions[?starts_with(FunctionName, '$PREFIX-')].FunctionName" --output text); do
    echo "delete function $f"; aws lambda delete-function --function-name "$f"
  done
  for p in "/aws/lambda/web/$PREFIX-" "/aws/lambda/$PREFIX-"; do
    for g in $(aws logs describe-log-groups --log-group-name-prefix "$p" --query 'logGroups[].logGroupName' --output text); do
      echo "delete log group $g"; aws logs delete-log-group --log-group-name "$g"
    done
  done
  if aws s3api head-bucket --bucket "$BUCKET" >/dev/null 2>&1; then
    echo "empty and delete bucket $BUCKET"
    python3 -I - "$BUCKET" <<'PY'
import json, subprocess, sys
b = sys.argv[1]
while True:
    out = json.loads(subprocess.check_output(["aws", "s3api", "list-object-versions", "--bucket", b, "--max-items", "900", "--output", "json"]) or b"{}")
    objs = [{"Key": o["Key"], "VersionId": o["VersionId"]} for o in out.get("Versions", []) + out.get("DeleteMarkers", [])]
    if not objs:
        break
    subprocess.check_call(["aws", "s3api", "delete-objects", "--bucket", b, "--delete", json.dumps({"Objects": objs, "Quiet": True})])
PY
    aws s3api delete-bucket --bucket "$BUCKET"
  fi
  aws dynamodb delete-table --table-name "$TABLE" >/dev/null 2>&1 && echo "delete table $TABLE" || true
  if aws iam get-role --role-name "$ROLE" >/dev/null 2>&1; then
    for p in $(aws iam list-role-policies --role-name "$ROLE" --query PolicyNames --output text); do
      aws iam delete-role-policy --role-name "$ROLE" --policy-name "$p"
    done
    aws iam delete-role --role-name "$ROLE" && echo "delete role $ROLE"
  fi
  echo "down: done (run again in ~7 min to catch recreated log groups)"
}

case "${1:-}" in up) up ;; down) down ;; *) echo "usage: $0 up|down" >&2; exit 2 ;; esac
