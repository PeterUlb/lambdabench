#!/bin/bash
# CloudFront in front of the `hello` Web Function: does CloudFront's time-to-first-byte see the
# caller wait that no Lambda metric reports? Needs `resources.sh up` and `web.sh create hello hello`.
# Caching is off (managed CachingDisabled policy); standard logs (v2) go to a CloudWatch log group
# in us-east-1, where CloudFront delivers them, with time-to-first-byte and origin-fbl.
#   cloudfront.sh create        distribution + log delivery; waits until deployed (~5 min)
#   cloudfront.sh run [cycles]  300 warm GETs via CloudFront, 300 direct, then per cycle a new
#                               revision and 6 GETs via CloudFront (?c=<cycle>); default 10 cycles
#   cloudfront.sh logs          fetch the log lines to results/cf-logs.json (they lag a few minutes)
#   cloudfront.sh down          delete the log delivery and group, disable and delete the distribution
source "$(dirname "$0")/common.sh"
NAME="$PREFIX-cf"
LG="/aws/cloudfront/$NAME"
USE1=(--region us-east-1)
# CachingDisabled managed cache policy
CACHE_POLICY=4135ea2d-6df8-44a3-9df3-4b5a84be39ad

create() {
  local origin cfg out id
  origin=$(url_of hello | sed 's#https://##')
  cfg=$(cat <<J
{"CallerReference":"$NAME-$(date +%s)","Comment":"$NAME","Enabled":true,"PriceClass":"PriceClass_100",
 "Origins":{"Quantity":1,"Items":[{"Id":"webfn","DomainName":"$origin","CustomOriginConfig":{"HTTPPort":80,"HTTPSPort":443,
  "OriginProtocolPolicy":"https-only","OriginSslProtocols":{"Quantity":1,"Items":["TLSv1.2"]},"OriginReadTimeout":30,"OriginKeepaliveTimeout":5}}]},
 "DefaultCacheBehavior":{"TargetOriginId":"webfn","ViewerProtocolPolicy":"https-only","CachePolicyId":"$CACHE_POLICY","Compress":false,
  "AllowedMethods":{"Quantity":2,"Items":["GET","HEAD"],"CachedMethods":{"Quantity":2,"Items":["GET","HEAD"]}}}}
J
)
  out=$(aws cloudfront create-distribution --distribution-config "$cfg" --query 'Distribution.[Id,DomainName]' --output text)
  read -r id dom <<<"$out"
  echo "$id" >"$OUT/cf.id"; echo "https://$dom" >"$OUT/cf.url"
  aws logs create-log-group --log-group-name "$LG" "${USE1[@]}"
  aws logs put-retention-policy --log-group-name "$LG" --retention-in-days 1 "${USE1[@]}"
  aws logs put-delivery-source --name "$NAME-src" --resource-arn "arn:aws:cloudfront::$ACCOUNT:distribution/$id" \
    --log-type ACCESS_LOGS "${USE1[@]}" >/dev/null
  local dst; dst=$(aws logs put-delivery-destination --name "$NAME-dst" "${USE1[@]}" \
    --delivery-destination-configuration "destinationResourceArn=arn:aws:logs:us-east-1:$ACCOUNT:log-group:$LG" \
    --query deliveryDestination.arn --output text)
  aws logs create-delivery --delivery-source-name "$NAME-src" --delivery-destination-arn "$dst" "${USE1[@]}" \
    --record-fields date time x-edge-location c-ip cs-uri-stem cs-uri-query sc-status x-edge-result-type \
      x-edge-request-id time-taken time-to-first-byte origin-fbl origin-lbl x-edge-detailed-result-type cs-protocol-version \
    --query delivery.id --output text >"$OUT/cf.delivery"
  aws cloudfront wait distribution-deployed --id "$id"
  echo "$id $(cat "$OUT/cf.url") -> $origin"
}

run() {
  local cf direct cycles="${1:-10}" out="$OUT/results/cf-cold.jsonl" c rid
  cf=$(cat "$OUT/cf.url"); direct=$(url_of hello)
  client_run cf-warm-cf "{\"action\":\"seq\",\"url\":\"$cf/run?w=1\",\"n\":300}"
  client_run cf-warm-direct "{\"action\":\"seq\",\"url\":\"$direct/run?w=2\",\"n\":300}"
  : >"$out"
  for c in $(seq 1 "$cycles"); do
    rid=$("$HERE/web.sh" revise hello)
    client_run cf-tmp "{\"action\":\"seq\",\"url\":\"$cf/run?c=$c\",\"n\":6}"
    python3 -I -c 'import json,sys; print(json.dumps({"cycle":int(sys.argv[2]),"rev":sys.argv[3],"reqs":json.load(open(sys.argv[1]))}))' \
      "$OUT/results/cf-tmp.json" "$c" "$rid" >>"$out"
  done
  rm -f "$OUT/results/cf-tmp.json"; echo "wrote $out; fetch the logs in a few minutes with \`cloudfront.sh logs\`"
}

logs() {
  aws logs filter-log-events --log-group-name "$LG" "${USE1[@]}" --query 'events[].message' --output json \
    >"$OUT/results/cf-logs.json"
  echo "$(python3 -I -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$OUT/results/cf-logs.json") lines" \
    "in results/cf-logs.json (a full run has 300 + 6 per cycle)"
}

down() {
  local id etag cfg
  aws logs delete-delivery --id "$(cat "$OUT/cf.delivery")" "${USE1[@]}" 2>/dev/null || true
  aws logs delete-delivery-source --name "$NAME-src" "${USE1[@]}" 2>/dev/null || true
  aws logs delete-delivery-destination --name "$NAME-dst" "${USE1[@]}" 2>/dev/null || true
  aws logs delete-log-group --log-group-name "$LG" "${USE1[@]}" 2>/dev/null || true
  for id in $(aws cloudfront list-distributions --query "DistributionList.Items[?Comment=='$NAME'].Id" --output text); do
    # A distribution must be disabled, and that change deployed, before it can be deleted.
    cfg=$(aws cloudfront get-distribution-config --id "$id" --output json)
    etag=$(echo "$cfg" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["ETag"])')
    if [ "$(echo "$cfg" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["DistributionConfig"]["Enabled"])')" = True ]; then
      etag=$(aws cloudfront update-distribution --id "$id" --if-match "$etag" --query ETag --output text \
        --distribution-config "$(echo "$cfg" | python3 -I -c 'import json,sys; d=json.load(sys.stdin)["DistributionConfig"]; d["Enabled"]=False; print(json.dumps(d))')")
    fi
    echo "disable distribution $id (~5 min)"; aws cloudfront wait distribution-deployed --id "$id"
    aws cloudfront delete-distribution --id "$id" --if-match "$etag" && echo "delete distribution $id"
  done
  rm -f "$OUT/cf.id" "$OUT/cf.url" "$OUT/cf.delivery"
}

cmd="${1:-}"; shift || true
case "$cmd" in create | run | logs | down) "$cmd" "$@" ;; *) sed -n '2,10p' "$0" >&2; exit 2 ;; esac
