#!/bin/bash
# Build, deploy, and measure the ecs-express-latency probe, then delete
# everything it created. See README.md in this directory. Usage:
#   oneoff/ecs-express-latency/run.sh [rounds] [requests-per-round]   (default 5 200)
#   oneoff/ecs-express-latency/run.sh teardown   (delete leftovers of a killed run)
#
# Needs docker (with arm64 emulation on an x86 host), curl, python3, and AWS
# credentials for the account to measure in. Resource names use the
# `oneoff-ecs-express` prefix, outside bencher's `lambdabench-` names, so
# `bencher teardown` never touches them and this script never touches the
# benchmark's resources. Teardown is by name, so it is safe to re-run.
set -euo pipefail

REGION="${AWS_REGION:-eu-central-1}"
MEMORY="${PROBE_MEMORY:-1769}" # 1769 MB = one full vCPU
PREFIX="oneoff-ecs-express"
CLUSTER="$PREFIX"
SERVICE="$PREFIX-server"
FN="$PREFIX-caller"
REPO_NAME="$PREFIX-server"
INFRA_ROLE="$PREFIX-infra"
EXEC_ROLE="$PREFIX-exec"
FN_ROLE="$PREFIX-lambda"
SERVER_LOGS="/oneoff/$PREFIX/server"
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
WORK="$REPO/.tmp/oneoff-ecs-express-latency"
export AWS_REGION="$REGION" AWS_PAGER=""

ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
SERVICE_ARN="arn:aws:ecs:$REGION:$ACCOUNT:service/$CLUSTER/$SERVICE"
mkdir -p "$WORK"

# Load balancers that exist in the region before this run; anything new after
# the service is created is the Express-managed ALB, which cleanup verifies is gone.
list_lbs() { aws elbv2 describe-load-balancers --query 'LoadBalancers[].LoadBalancerArn' --output text | tr '\t' '\n' | sort; }

# ECS service status by name: ACTIVE / DRAINING / INACTIVE, None when missing,
# ERROR when the call itself failed (so a throttle never reads as "gone").
service_status() {
  local out
  if out=$(aws ecs describe-services --cluster "$CLUSTER" --services "$SERVICE" \
    --query 'services[0].status' --output text 2>&1); then
    echo "$out"
  else
    case "$out" in *ClusterNotFound*) echo None ;; *) echo ERROR ;; esac
  fi
}

cleanup() {
  set +e
  echo "+ cleanup"
  # Recorded before deleting anything, so a run that died mid-deploy is covered too.
  if [ -s "$WORK/lbs-before.txt" ]; then
    comm -13 "$WORK/lbs-before.txt" <(list_lbs) >"$WORK/new-lbs.txt"
  fi
  if aws ecs delete-express-gateway-service --service-arn "$SERVICE_ARN" >/dev/null 2>&1; then
    echo "  deleting the Express service (ALB, target groups, security groups); this takes minutes"
  fi
  local status=ERROR
  for _ in $(seq 1 120); do
    status=$(service_status)
    case "$status" in INACTIVE | None) break ;; esac
    sleep 10
  done
  # ECS deletes the managed ALB with the infrastructure role, so the role stays
  # until the ALB recorded at deploy time is gone.
  local left=""
  if [ -s "$WORK/new-lbs.txt" ]; then
    for _ in $(seq 1 60); do
      left=$(comm -12 "$WORK/new-lbs.txt" <(list_lbs))
      [ -z "$left" ] && break
      sleep 10
    done
  fi
  case "$status" in
  INACTIVE | None)
    aws ecs delete-cluster --cluster "$CLUSTER" >/dev/null 2>&1
    aws iam detach-role-policy --role-name "$INFRA_ROLE" \
      --policy-arn arn:aws:iam::aws:policy/service-role/AmazonECSInfrastructureRoleforExpressGatewayServices 2>/dev/null
    aws iam delete-role --role-name "$INFRA_ROLE" 2>/dev/null
    ;;
  *)
    echo "!! Express service $SERVICE_ARN is still $status after 20 min; kept cluster $CLUSTER and" \
      "role $INFRA_ROLE so ECS can finish deleting it. Re-run: $0 teardown" >&2
    ;;
  esac
  # Deleting the service leaves the task definition Express registered for it
  # (family `<cluster>-<service>`) ACTIVE; deregister and delete every revision.
  for td in $(aws ecs list-task-definitions --family-prefix "$CLUSTER-$SERVICE" \
    --query taskDefinitionArns --output text 2>/dev/null); do
    aws ecs deregister-task-definition --task-definition "$td" >/dev/null 2>&1
    aws ecs delete-task-definitions --task-definitions "$td" >/dev/null 2>&1
  done
  aws lambda delete-function --function-name "$FN" >/dev/null 2>&1
  aws logs delete-log-group --log-group-name "/aws/lambda/$FN" 2>/dev/null
  aws logs delete-log-group --log-group-name "$SERVER_LOGS" 2>/dev/null
  aws ecr delete-repository --repository-name "$REPO_NAME" --force >/dev/null 2>&1
  rm -rf "$WORK/docker-config"
  aws iam detach-role-policy --role-name "$EXEC_ROLE" \
    --policy-arn arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy 2>/dev/null
  aws iam delete-role --role-name "$EXEC_ROLE" 2>/dev/null
  aws iam detach-role-policy --role-name "$FN_ROLE" \
    --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole 2>/dev/null
  aws iam delete-role --role-name "$FN_ROLE" 2>/dev/null
  if [ -n "$left" ]; then
    echo "!! load balancer(s) created during the run still exist: $left" >&2
  elif [ -s "$WORK/new-lbs.txt" ]; then
    echo "  verified: the Express-managed load balancer is gone"
  fi
  [ -z "$left" ] && rm -f "$WORK/new-lbs.txt" "$WORK/lbs-before.txt"
}

if [ "${1:-}" = teardown ]; then
  cleanup
  exit 0
fi

ROUNDS="${1:-5}"
PER_ROUND="${2:-200}"

echo "+ build (arm64, inside amazonlinux:2023)"
docker run --rm --platform linux/arm64 -v "$REPO:/repo" -w /repo/oneoff/ecs-express-latency amazonlinux:2023 bash -c '
  set -e
  dnf -q -y install gcc zip >/dev/null
  curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal >/dev/null
  . ~/.cargo/env
  out=/repo/.tmp/oneoff-ecs-express-latency
  export CARGO_TARGET_DIR=$out/target-arm64
  cargo build -q --locked --release
  cp "$CARGO_TARGET_DIR/release/server" "$out/server"
  rm -f "$out/bootstrap.zip"
  (cd "$CARGO_TARGET_DIR/release" && zip -q "$out/bootstrap.zip" bootstrap)
  # The container runs as root; give the build output back to the repo owner.
  chown -R "$(stat -c %u:%g /repo)" "$out"'
printf 'FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal\nCOPY server /server\nENTRYPOINT ["/server"]\n' \
  >"$WORK/Dockerfile"
image="$ACCOUNT.dkr.ecr.$REGION.amazonaws.com/$REPO_NAME:latest"
docker build -q --platform linux/arm64 -t "$image" "$WORK" >/dev/null

trap cleanup EXIT
trap 'exit 130' INT TERM

echo "+ deploy ($REGION; Fargate arm64 1 vCPU behind ECS Express; caller Lambda arm64 $MEMORY MB)"
list_lbs >"$WORK/lbs-before.txt"
role() { # name service policy-arn -> prints the role ARN
  aws iam create-role --role-name "$1" --query Role.Arn --output text \
    --assume-role-policy-document "{\"Version\":\"2012-10-17\",\"Statement\":[{\"Effect\":\"Allow\",\"Principal\":{\"Service\":\"$2\"},\"Action\":\"sts:AssumeRole\"}]}"
  aws iam attach-role-policy --role-name "$1" --policy-arn "$3"
}
infra_arn=$(role "$INFRA_ROLE" ecs.amazonaws.com \
  arn:aws:iam::aws:policy/service-role/AmazonECSInfrastructureRoleforExpressGatewayServices)
exec_arn=$(role "$EXEC_ROLE" ecs-tasks.amazonaws.com \
  arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy)
fn_role_arn=$(role "$FN_ROLE" lambda.amazonaws.com \
  arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole)

aws ecr create-repository --repository-name "$REPO_NAME" >/dev/null
# A throwaway Docker config with the credential helper disabled: the ECR token is
# too long for some helpers (WSL's Windows credential store rejects it). An empty
# `credsStore` must be explicit; with none set, Docker Desktop's WSL CLI falls back
# to desktop.exe. Deleted on cleanup.
mkdir -p "$WORK/docker-config"
echo '{"credsStore":""}' >"$WORK/docker-config/config.json"
aws ecr get-login-password |
  DOCKER_CONFIG="$WORK/docker-config" docker login -u AWS --password-stdin "${image%%/*}" >/dev/null
DOCKER_CONFIG="$WORK/docker-config" docker push -q "$image" >/dev/null
aws logs create-log-group --log-group-name "$SERVER_LOGS"
aws logs put-retention-policy --log-group-name "$SERVER_LOGS" --retention-in-days 1
aws ecs create-cluster --cluster-name "$CLUSTER" >/dev/null

# min = max = 1 task, so every request hits the same task and scaling never
# adds a cold one mid-measurement. Retried because a just-created role can be
# rejected until IAM propagates.
for attempt in $(seq 1 8); do
  if aws ecs create-express-gateway-service --cluster "$CLUSTER" --service-name "$SERVICE" \
    --infrastructure-role-arn "$infra_arn" --execution-role-arn "$exec_arn" \
    --cpu 1024 --memory 2048 --cpu-architecture ARM64 \
    --scaling-target minTaskCount=1,maxTaskCount=1 \
    --primary-container "image=$image,containerPort=80,awsLogsConfiguration={logGroup=$SERVER_LOGS,logStreamPrefix=ecs}" \
    >/dev/null 2>"$WORK/create.err"; then
    break
  fi
  [ "$attempt" = 8 ] && { cat "$WORK/create.err" >&2; exit 1; }
  sleep 15
done

aws lambda create-function --function-name "$FN" --runtime provided.al2023 \
  --architectures arm64 --handler bootstrap --memory-size "$MEMORY" --timeout 60 \
  --role "$fn_role_arn" --zip-file "fileb://$WORK/bootstrap.zip" >/dev/null 2>"$WORK/fn.err" || {
  sleep 15 # IAM propagation, as above
  aws lambda create-function --function-name "$FN" --runtime provided.al2023 \
    --architectures arm64 --handler bootstrap --memory-size "$MEMORY" --timeout 60 \
    --role "$fn_role_arn" --zip-file "fileb://$WORK/bootstrap.zip" >/dev/null
}

echo "+ wait for the endpoint to serve (ALB, DNS and certificate; usually several minutes)"
endpoint=""
for _ in $(seq 1 120); do
  endpoint=$(aws ecs describe-express-gateway-service --service-arn "$SERVICE_ARN" \
    --query "service.activeConfigurations[0].ingressPaths[?accessType=='PUBLIC'].endpoint | [0]" \
    --output text 2>/dev/null || true)
  if [ -n "$endpoint" ] && [ "$endpoint" != None ]; then
    [[ "$endpoint" == *://* ]] || endpoint="https://$endpoint"
    [ "$(curl -s -m 5 "$endpoint/" || true)" = hello ] && break
  fi
  sleep 10
done
[ "$(curl -s -m 5 "$endpoint/" || true)" = hello ] || { echo "endpoint never served: '$endpoint'" >&2; exit 1; }
echo "  $endpoint"

aws lambda wait function-active-v2 --function-name "$FN"
aws lambda update-function-configuration --function-name "$FN" \
  --environment "Variables={TARGET_URL=$endpoint/}" >/dev/null
aws lambda wait function-updated-v2 --function-name "$FN"

out="$WORK/results-$(date +%Y%m%d-%H%M%S).jsonl"
echo "+ measure (1 warm-up invoke of 50, then $ROUNDS x $PER_ROUND sequential GETs) -> $out"
invoke() { # n -> appends the response line to $out
  aws lambda invoke --function-name "$FN" --cli-binary-format raw-in-base64-out \
    --payload "$1" "$WORK/response.json" --query FunctionError --output text >"$WORK/fnerr.txt"
  if [ "$(cat "$WORK/fnerr.txt")" != None ]; then
    cat "$WORK/response.json" >&2
    exit 1
  fi
  printf '%s\t%s\n' "$2" "$(cat "$WORK/response.json")" >>"$out"
}
invoke 50 warmup
for round in $(seq 1 "$ROUNDS"); do
  invoke "$PER_ROUND" "round-$round"
done

python3 -I - "$out" <<'EOF'
import json, sys
rows = [line.split("\t", 1) for line in open(sys.argv[1])]
first = json.loads(rows[0][1])["first_ms"]
timed = [json.loads(r) for tag, r in rows if tag != "warmup"]
s = sorted(x for r in timed for x in r["samples"])
pct = lambda p: s[min(len(s) - 1, int(p / 100 * len(s)))]
print(f"  first request (new TCP + TLS connection): {first:.2f} ms")
print(f"  {len(s)} requests on the pooled connection ({timed[0]['version']}):")
print(f"    p50 {pct(50):.2f}  p90 {pct(90):.2f}  p99 {pct(99):.2f}  min {s[0]:.2f}  max {s[-1]:.2f} ms")
EOF
