#!/usr/bin/env bash
# Kick off one on-demand benchmark + publish run on ECS Fargate.
#
# Reads the cluster, task definition, and public subnets from the deployed
# LambdaBenchRunnerStack CloudFormation outputs, then launches a single task. The task
# self-terminates when run-benchmark.sh completes (hours-long run). Follow progress
# in the CloudWatch log group "/lambdabench/runner" (stream prefix "lambdabench-runner").
#
# Usage:
#   deploy/run.sh                 # launch with the task's default env
#   KEEP_RESOURCES=1 deploy/run.sh  # leave the function matrix deployed after the run
#
# Requires: awscli v2, credentials for the target account, region eu-central-1.
set -euo pipefail

REGION="eu-central-1"
RUNNER_STACK="LambdaBenchRunnerStack"

get_output() {
  aws cloudformation describe-stacks --region "$REGION" \
    --stack-name "$RUNNER_STACK" \
    --query "Stacks[0].Outputs[?OutputKey=='$1'].OutputValue" --output text
}

CLUSTER="$(get_output ClusterName)"
TASK_DEF="$(get_output TaskDefinitionArn)"
SUBNETS="$(get_output PublicSubnetIds)"

if [[ -z "$CLUSTER" || -z "$TASK_DEF" || -z "$SUBNETS" ]]; then
  echo "error: could not read LambdaBenchRunnerStack outputs (is it deployed?)" >&2
  exit 1
fi

# Optional per-run override forwarded into the container.
OVERRIDES='{}'
if [[ "${KEEP_RESOURCES:-0}" == "1" ]]; then
  OVERRIDES='{"containerOverrides":[{"name":"Runner","environment":[{"name":"KEEP_RESOURCES","value":"1"}]}]}'
fi

echo "Launching benchmark task on cluster $CLUSTER ..."
aws ecs run-task --region "$REGION" \
  --cluster "$CLUSTER" \
  --task-definition "$TASK_DEF" \
  --launch-type FARGATE \
  --count 1 \
  --network-configuration "awsvpcConfiguration={subnets=[${SUBNETS}],assignPublicIp=ENABLED}" \
  --overrides "$OVERRIDES" \
  --query 'tasks[0].taskArn' --output text

echo "Task launched. Tail logs in CloudWatch (log group /lambdabench/runner, stream prefix lambdabench-runner)."
