// Probe copy of the matrix's Go oneclient. GB_PREREAD=exe reads the executable
// once, first thing in main, before the SDK config load. The first request
// prints one PROBE line with CPU, page-fault and disk-read counters for init
// (from main's start) and for the request.
package main

import (
	"context"
	"crypto/x509"
	"fmt"
	"io"
	"log"
	"os"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/aws/aws-lambda-go/lambda"
	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb/types"
)

type counters struct {
	majflt, minflt, readKB int64
	cpu                    time.Duration
	wall                   time.Time
}

func now() counters {
	var ru syscall.Rusage
	_ = syscall.Getrusage(syscall.RUSAGE_SELF, &ru)
	c := counters{
		majflt: ru.Majflt,
		minflt: ru.Minflt,
		cpu:    time.Duration(ru.Utime.Nano() + ru.Stime.Nano()),
		wall:   time.Now(),
	}
	if b, err := os.ReadFile("/proc/self/io"); err == nil {
		for _, line := range strings.Split(string(b), "\n") {
			if v, ok := strings.CutPrefix(line, "read_bytes: "); ok {
				n, _ := strconv.ParseInt(v, 10, 64)
				c.readKB = n / 1024
			}
		}
	}
	return c
}

func ms(d time.Duration) float64 { return float64(d.Microseconds()) / 1000 }

type state struct {
	ddb        *dynamodb.Client
	table, key string
	start      counters
	init       counters
	prereadMS  float64
	prereadKB  int64
	exeKB      int64
	n          int
}

type response struct {
	Scenario string `json:"scenario"`
	N        int    `json:"n"`
}

func (s *state) handle(ctx context.Context) (response, error) {
	before := now()
	out, err := s.ddb.GetItem(ctx, &dynamodb.GetItemInput{
		TableName: &s.table,
		Key:       map[string]types.AttributeValue{"pk": &types.AttributeValueMemberS{Value: s.key}},
	})
	if err != nil {
		return response{}, err
	}
	if out.Item == nil {
		return response{}, fmt.Errorf("seeded item not found for key %s", s.key)
	}
	after := now()
	s.n++
	if s.n == 1 {
		roots := -1
		if pool, err := x509.SystemCertPool(); err == nil {
			//lint:ignore SA1019 only counting
			roots = len(pool.Subjects())
		}
		fmt.Fprintf(os.Stderr, "PROBE {\"n\":1,\"roots\":%d,\"exe_kb\":%d,\"preread_ms\":%.3f,\"preread_kb\":%d,"+
			"\"init_ms\":%.3f,\"init_cpu_ms\":%.3f,\"init_majflt\":%d,\"init_minflt\":%d,\"init_read_kb\":%d,"+
			"\"start_majflt\":%d,\"start_read_kb\":%d,"+
			"\"handler_ms\":%.3f,\"handler_cpu_ms\":%.3f,\"handler_majflt\":%d,\"handler_minflt\":%d,\"handler_read_kb\":%d}\n",
			roots, s.exeKB, s.prereadMS, s.prereadKB,
			ms(s.init.wall.Sub(s.start.wall)), ms(s.init.cpu-s.start.cpu),
			s.init.majflt-s.start.majflt, s.init.minflt-s.start.minflt, s.init.readKB-s.start.readKB,
			s.start.majflt, s.start.readKB,
			ms(after.wall.Sub(before.wall)), ms(after.cpu-before.cpu),
			after.majflt-before.majflt, after.minflt-before.minflt, after.readKB-before.readKB)
	}
	return response{Scenario: "oneclient", N: s.n}, nil
}

func main() {
	s := &state{start: now()}
	if os.Getenv("GB_STARTONLY") == "1" {
		pg := pagesJSON("main")
		st := startStat()
		if n, err := strconv.Atoi(os.Getenv("GB_TOUCH")); err == nil {
			st = st[:len(st)-1] + `,"touch":` + touchBlocks(n) + "}"
		}
		os.Stderr.WriteString(pg + "\n")
		os.Stderr.WriteString("PROBE " + st + "\n")
		lambda.Start(func(context.Context) (string, error) { return st, nil })
		return
	}
	if os.Getenv("GB_PREREAD") == "exe" {
		t := time.Now()
		if f, err := os.Open("/proc/self/exe"); err == nil {
			n, _ := io.Copy(io.Discard, f)
			f.Close()
			s.prereadKB = n / 1024
		}
		s.prereadMS = ms(time.Since(t))
	}
	if fi, err := os.Stat("/proc/self/exe"); err == nil {
		s.exeKB = fi.Size() / 1024
	}
	cfg, err := config.LoadDefaultConfig(context.Background(),
		config.WithRetryer(func() aws.Retryer { return aws.NopRetryer{} }))
	if err != nil {
		log.Fatalf("loading AWS config: %v", err)
	}
	s.ddb = dynamodb.NewFromConfig(cfg)
	s.table = os.Getenv("LAMBDABENCH_TABLE")
	s.key = os.Getenv("LAMBDABENCH_KEY")
	if s.table == "" || s.key == "" {
		log.Fatal("LAMBDABENCH_TABLE and LAMBDABENCH_KEY must be set")
	}
	// GB_LOCALWARM=1: the tuning page's local warm-up (roots + credentials).
	if os.Getenv("GB_LOCALWARM") == "1" {
		if _, err := x509.SystemCertPool(); err != nil {
			log.Fatal(err)
		}
		if _, err := cfg.Credentials.Retrieve(context.Background()); err != nil {
			log.Fatal(err)
		}
	}
	// GB_PREWARM=1: one real call at init, so the TLS connection, the cert
	// pool and the request path are set up before the first invoke.
	if os.Getenv("GB_PREWARM") == "1" {
		_, _ = s.ddb.GetItem(context.Background(), &dynamodb.GetItemInput{
			TableName: &s.table,
			Key:       map[string]types.AttributeValue{"pk": &types.AttributeValueMemberS{Value: s.key}},
		})
	}
	s.init = now()
	lambda.Start(s.handle)
}
