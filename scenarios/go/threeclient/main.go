// Scenario "threeclient": construct THREE AWS SDK clients (DynamoDB, KMS, S3) at
// init and call all three per invoke.
//
// Each client is built during the Lambda init phase and reused across warm
// invokes. At invoke time the handler performs three operations: a DynamoDB
// GetItem, a KMS Encrypt of a short constant, and an S3 GetObject of a small
// seeded object. Comparing its cold start against `oneclient` shows what
// additional AWS clients add (extra middleware stacks plus a first TLS handshake
// per distinct endpoint). Read by direct comparison, not subtraction.
package main

import (
	"context"
	"fmt"
	"io"
	"log"
	"os"

	"github.com/aws/aws-lambda-go/lambda"
	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb"
	ddbtypes "github.com/aws/aws-sdk-go-v2/service/dynamodb/types"
	"github.com/aws/aws-sdk-go-v2/service/kms"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

// Environment variables injected by the bencher at deploy time.
const (
	envTable     = "LAMBDABENCH_TABLE"
	envKey       = "LAMBDABENCH_KEY"
	envKMSKey    = "LAMBDABENCH_KMS_KEY_ID"
	envBucket    = "LAMBDABENCH_BUCKET"
	envObjectKey = "LAMBDABENCH_OBJECT_KEY"
)

// state holds the three clients plus the resource identifiers, built once at init.
type state struct {
	ddb       *dynamodb.Client
	kms       *kms.Client
	s3        *s3.Client
	table     string
	key       string
	kmsKeyID  string
	bucket    string
	objectKey string
}

type response struct {
	Scenario         string  `json:"scenario"`
	DDBPayload       *string `json:"ddb_payload"`
	KMSCiphertextLen int     `json:"kms_ciphertext_len"`
	S3ObjectLen      int     `json:"s3_object_len"`
}

// handle calls DynamoDB GetItem, KMS Encrypt, and S3 GetObject, returning a
// small summary so the benchmark can confirm all three succeeded.
func (s *state) handle(ctx context.Context) (response, error) {
	// 1. DynamoDB GetItem.
	ddbOut, err := s.ddb.GetItem(ctx, &dynamodb.GetItemInput{
		TableName: &s.table,
		Key:       map[string]ddbtypes.AttributeValue{"pk": &ddbtypes.AttributeValueMemberS{Value: s.key}},
	})
	if err != nil {
		return response{}, err
	}
	// Fail loud if the seeded item is absent: a missing item means a broken
	// benchmark setup, never a null fallback (matches the other languages).
	if ddbOut.Item == nil {
		return response{}, fmt.Errorf("seeded item not found for key %s", s.key)
	}
	var payload *string
	if v, ok := ddbOut.Item["payload"].(*ddbtypes.AttributeValueMemberS); ok {
		payload = &v.Value
	}

	// 2. KMS Encrypt of a short constant ("hello").
	kmsOut, err := s.kms.Encrypt(ctx, &kms.EncryptInput{
		KeyId:     &s.kmsKeyID,
		Plaintext: []byte("hello"),
	})
	if err != nil {
		return response{}, err
	}

	// 3. S3 GetObject of a small seeded object. Measure the raw byte length; a
	// decode to a string would add per-invoke work the other languages do not do.
	s3Out, err := s.s3.GetObject(ctx, &s3.GetObjectInput{
		Bucket: &s.bucket,
		Key:    &s.objectKey,
	})
	if err != nil {
		return response{}, err
	}
	defer s3Out.Body.Close()
	body, err := io.ReadAll(s3Out.Body)
	if err != nil {
		return response{}, err
	}

	return response{
		Scenario:         "threeclient",
		DDBPayload:       payload,
		KMSCiphertextLen: len(kmsOut.CiphertextBlob),
		S3ObjectLen:      len(body),
	}, nil
}

func main() {
	// Init phase: resolve config once, then build all three clients. Retries
	// disabled (aws.NopRetryer): a throttle/transient must surface as a hard
	// failure, not be silently retried into an inflated Duration. A failed run
	// beats wrong data.
	cfg, err := config.LoadDefaultConfig(context.Background(),
		config.WithRetryer(func() aws.Retryer { return aws.NopRetryer{} }))
	if err != nil {
		log.Fatalf("loading AWS config: %v", err)
	}
	s := &state{
		ddb:       dynamodb.NewFromConfig(cfg),
		kms:       kms.NewFromConfig(cfg),
		s3:        s3.NewFromConfig(cfg),
		table:     os.Getenv(envTable),
		key:       os.Getenv(envKey),
		kmsKeyID:  os.Getenv(envKMSKey),
		bucket:    os.Getenv(envBucket),
		objectKey: os.Getenv(envObjectKey),
	}
	if s.table == "" || s.key == "" || s.kmsKeyID == "" || s.bucket == "" || s.objectKey == "" {
		log.Fatalf("%s, %s, %s, %s, %s must all be set",
			envTable, envKey, envKMSKey, envBucket, envObjectKey)
	}
	lambda.Start(s.handle)
}
