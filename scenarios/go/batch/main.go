// Scenario "batch": a deserialize-heavy batch record processor (the canonical
// Kinesis/SQS-consumer shape).
//
// At init the handler fetches a large (~16 MB) JSON array of event records from
// S3 and keeps the raw bytes in memory. Each warm invoke parses the whole batch
// into records, then groups-by `key` into a map of running sum + count, and
// returns the per-group totals.
//
// Read this scenario on two independent axes:
//   - MEDIAN = each language's standard JSON-parser speed, which dominates. Go
//     decodes with the reflection-based stdlib `encoding/json` into a typed
//     []record, several times slower than Rust's compile-time-monomorphized
//     serde; that parse, not the group-by, is essentially all of Go's batch
//     median. Decoding into a typed struct slice (not a generic map tree) keeps
//     the live graph representative. We keep `encoding/json` because it is Go's
//     standard parser; a faster third-party decoder would compare libraries.
//   - TAIL (P99/P99.9) at the smaller memory tiers = allocation + GC. The parsed
//     records and the group map are all live simultaneously for the whole invoke,
//     a large transient object graph a tracing GC must trace then collect, where a
//     non-GC runtime drops it at end of scope. Go's concurrent low-pause collector
//     keeps this tail mild (more pronounced on Java/Python).
//
// Contrast lettercount, which counts into a fixed [26]uint64 (nothing grows).
// Fetching the batch at init keeps the warm measurement pure compute. The
// group-by (parse + map insert/update + arithmetic) is in-language work, not a
// native library, so the comparison is fair.
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
	envObject = "LAMBDABENCH_BATCH_KEY"
)

// record is one event in the batch. Decoded into a typed struct (not a generic
// map tree) so the live object graph matches the other typed languages.
type record struct {
	Key   string `json:"key"`
	Value int64  `json:"value"`
}

// agg is the running aggregate per group.
type agg struct {
	sum   int64
	count uint64
}

// state holds the raw batch bytes fetched once at init and reused across warm
// invokes.
type state struct {
	payload []byte
}

// group is one entry in the per-group summary emitted in the response.
type group struct {
	Key   string `json:"key"`
	Sum   int64  `json:"sum"`
	Count uint64 `json:"count"`
}

type response struct {
	Scenario string  `json:"scenario"`
	Records  int     `json:"records"`
	Groups   int     `json:"groups"`
	Total    int64   `json:"total"`
	PerGroup []group `json:"per_group"`
}

// processBatch parses the whole batch and groups-by `key` into sum + count. The
// parse allocates the full record graph and the map holds an entry per distinct
// key. Both live for the duration of the call, which is the GC fuel.
func processBatch(payload []byte) (response, error) {
	var records []record
	if err := json.Unmarshal(payload, &records); err != nil {
		return response{}, err
	}
	groups := make(map[string]*agg)
	var total int64
	for i := range records {
		r := &records[i]
		a := groups[r.Key]
		if a == nil {
			a = &agg{}
			groups[r.Key] = a
		}
		a.sum += r.Value
		a.count++
		total += r.Value
	}
	// Emit a compact summary: per-group totals plus headline figures. Building
	// the output slice is itself allocation proportional to group count,
	// mirroring what a real batch processor would hand downstream.
	perGroup := make([]group, 0, len(groups))
	for k, a := range groups {
		perGroup = append(perGroup, group{Key: k, Sum: a.sum, Count: a.count})
	}
	return response{
		Scenario: "batch",
		Records:  len(records),
		Groups:   len(groups),
		Total:    total,
		PerGroup: perGroup,
	}, nil
}

func (s *state) handle(_ context.Context) (response, error) {
	return processBatch(s.payload)
}

func main() {
	// Init phase: fetch the batch from S3 once and hold it in memory. Retries
	// disabled (aws.NopRetryer): a throttle on the init-time fetch must surface as
	// a hard failure, not be silently retried into an inflated init_ms. A failed
	// run beats wrong data.
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
		log.Fatalf("fetching batch payload: %v", err)
	}
	defer obj.Body.Close()
	payload, err := io.ReadAll(obj.Body)
	if err != nil {
		log.Fatalf("reading batch payload: %v", err)
	}
	s := &state{payload: payload}
	lambda.Start(s.handle)
}
