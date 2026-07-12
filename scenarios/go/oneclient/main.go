// Scenario "oneclient": construct ONE AWS SDK client (DynamoDB) at init and call
// it once (GetItem) per invoke.
//
// The DynamoDB client is constructed during the Lambda init phase (before the
// handler loop starts) so the cold-start measurement includes AWS config
// resolution and client construction, matching how a real service is written.
// The AWS SDK for Go v2 is bundled into the binary (Go links statically),
// matching Rust's static linking and the other languages' bundled SDKs.
package main

import (
	"context"
	"fmt"
	"log"
	"os"

	"github.com/aws/aws-lambda-go/lambda"
	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb/types"
)

// Environment variables injected by the bencher at deploy time.
const (
	envTable = "LAMBDABENCH_TABLE"
	envKey   = "LAMBDABENCH_KEY"
)

// state holds the client and resource identifiers, built once at init.
type state struct {
	ddb   *dynamodb.Client
	table string
	key   string
}

type response struct {
	Scenario string  `json:"scenario"`
	Key      string  `json:"key"`
	Payload  *string `json:"payload"`
}

// handle reads the seeded item by its partition key and returns its attributes.
func (s *state) handle(ctx context.Context) (response, error) {
	out, err := s.ddb.GetItem(ctx, &dynamodb.GetItemInput{
		TableName: &s.table,
		Key:       map[string]types.AttributeValue{"pk": &types.AttributeValueMemberS{Value: s.key}},
	})
	if err != nil {
		return response{}, err
	}
	if out.Item == nil {
		// Fail loud if the seeded item is absent: a missing item means a broken
		// benchmark setup, never a null fallback (matches the other languages).
		return response{}, fmt.Errorf("seeded item not found for key %s", s.key)
	}
	var payload *string
	if v, ok := out.Item["payload"].(*types.AttributeValueMemberS); ok {
		payload = &v.Value
	}
	return response{Scenario: "oneclient", Key: s.key, Payload: payload}, nil
}

func main() {
	// Init phase: resolve config and build the client once, reused across warm
	// invokes. Retries disabled (aws.NopRetryer): a throttle/transient must
	// surface as a hard failure, not be silently retried into an inflated
	// Duration. A failed run beats wrong data.
	cfg, err := config.LoadDefaultConfig(context.Background(),
		config.WithRetryer(func() aws.Retryer { return aws.NopRetryer{} }))
	if err != nil {
		log.Fatalf("loading AWS config: %v", err)
	}
	s := &state{
		ddb:   dynamodb.NewFromConfig(cfg),
		table: os.Getenv(envTable),
		key:   os.Getenv(envKey),
	}
	if s.table == "" || s.key == "" {
		log.Fatalf("%s and %s must be set", envTable, envKey)
	}
	lambda.Start(s.handle)
}
