// Scenario "lettercount": CPU-bound work with no per-invoke I/O.
//
// At init the handler fetches a JSON document from S3 once (a ~1 MB array of
// ASCII strings) and keeps the raw bytes in memory. Each warm invoke does pure
// in-memory work: parse the JSON array, and for every string count the
// occurrences of each lowercase ASCII letter (a..z), summing into 26 per-letter
// totals returned as the response.
//
// Why this workload:
//  1. It is in-language CPU work. The counting is a tight loop over each string's
//     bytes, running in compiled Go machine code (vs Rust machine code / the V8
//     JIT / the CPython interpreter) rather than a shared native library. A hashing
//     or JSON-stringify-heavy workload would instead spend most time in native
//     C/OpenSSL every runtime shares, measuring the library not the language.
//  2. json.Unmarshal rebuilds a fresh object graph each invoke, so under a
//     constrained heap a GC'd runtime may show pauses in the warm tail; Go has a
//     tracing GC, so this measures Go's collector too.
//
// Fetching the payload at init keeps the warm measurement pure compute. Counting
// is restricted to ASCII a..z so every language does identical work and produces
// identical totals; iterating bytes (not runes) matches Rust's byte loop, correct
// and fastest for ASCII-only input.
package main

import (
	"context"
	"encoding/json"
	"io"
	"log"
	"os"

	"github.com/aws/aws-lambda-go/lambda"
	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

// Environment variables injected by the bencher at deploy time.
const (
	envBucket = "LAMBDABENCH_BUCKET"
	envObject = "LAMBDABENCH_LETTERCOUNT_KEY"
)

// state holds the payload fetched once at init and reused across warm invokes.
type state struct {
	payload []byte
}

type response struct {
	Scenario     string     `json:"scenario"`
	LetterCounts [26]uint64 `json:"letter_counts"`
}

// countLetters parses the JSON string array and counts lowercase-ASCII letters
// across all entries. Returns 26 totals (index 0 = 'a' .. 25 = 'z'). The parse
// is the allocation source; the per-byte count is the in-language CPU work.
//
// Decoded into a typed []string (not a generic interface{} tree) so the parse's
// object graph matches the other languages, whose parsers produce a flat string
// array. Same fairness rule as the batch handler.
func countLetters(payload []byte) ([26]uint64, error) {
	var entries []string
	if err := json.Unmarshal(payload, &entries); err != nil {
		return [26]uint64{}, err
	}
	var totals [26]uint64
	for _, s := range entries {
		for i := 0; i < len(s); i++ {
			b := s[i]
			if b >= 'a' && b <= 'z' {
				totals[b-'a']++
			}
		}
	}
	return totals, nil
}

func (s *state) handle(_ context.Context) (response, error) {
	totals, err := countLetters(s.payload)
	if err != nil {
		return response{}, err
	}
	return response{Scenario: "lettercount", LetterCounts: totals}, nil
}

func main() {
	// Init phase: fetch the JSON payload from S3 once and hold it in memory.
	// Retries disabled (aws.NopRetryer): a throttle on the init-time fetch must
	// surface as a hard failure, not be silently retried into an inflated init_ms.
	// A failed run beats wrong data.
	ctx := context.Background()
	cfg, err := config.LoadDefaultConfig(ctx,
		config.WithRetryer(func() aws.Retryer { return aws.NopRetryer{} }))
	if err != nil {
		log.Fatalf("loading AWS config: %v", err)
	}
	bucket := os.Getenv(envBucket)
	key := os.Getenv(envObject)
	if bucket == "" || key == "" {
		log.Fatalf("%s and %s must be set", envBucket, envObject)
	}
	client := s3.NewFromConfig(cfg)
	obj, err := client.GetObject(ctx, &s3.GetObjectInput{Bucket: &bucket, Key: &key})
	if err != nil {
		log.Fatalf("fetching lettercount payload: %v", err)
	}
	defer obj.Body.Close()
	payload, err := io.ReadAll(obj.Body)
	if err != nil {
		log.Fatalf("reading lettercount payload: %v", err)
	}
	s := &state{payload: payload}
	lambda.Start(s.handle)
}
