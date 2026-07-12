// Scenario "hello": return a constant.
//
// Baseline isolating Go runtime startup + handler dispatch, with no I/O and no
// SDK initialization. Uses the conventional aws-lambda-go entrypoint
// (lambda.Start) on the provided.al2023 custom runtime, matching how a real Go
// handler is written, so this is the startup floor for a conventional handler.
package main

import (
	"context"

	"github.com/aws/aws-lambda-go/lambda"
)

// response is the constant payload, byte-for-byte equivalent to the other
// languages' hello output.
type response struct {
	Message  string `json:"message"`
	Scenario string `json:"scenario"`
}

// handler ignores its input and returns the constant payload.
func handler(_ context.Context) (response, error) {
	return response{Message: "hello", Scenario: "hello"}, nil
}

func main() {
	lambda.Start(handler)
}
