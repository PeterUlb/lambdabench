// Scenario "hello": return a constant.
//
// Baseline isolating Go runtime startup + handler dispatch, with no I/O and no
// SDK initialization. Uses the conventional aws-lambda-go entrypoint
// (lambda.Start) on the provided.al2023 custom runtime, so this measures the
// startup floor for a Go handler written the usual way.
package main

import (
	"context"

	"github.com/aws/aws-lambda-go/lambda"
	"github.com/lambdabench/go-artifacts/internal/cpuinfo"
)

// response is the constant payload, with the same fields and values as the
// other languages' hello output (key order can differ: Java's Map.of does not
// guarantee one).
type response struct {
	Message  string `json:"message"`
	Scenario string `json:"scenario"`
}

func handler(ctx context.Context) (response, error) {
	cpuinfo.ReportIfRequested(ctx)
	return response{Message: "hello", Scenario: "hello"}, nil
}

func main() {
	lambda.Start(handler)
}
