// Minimal Go function (aws-lambda-go only, no SDK) that reports startStat.
package main

import (
	"context"
	"os"

	"github.com/aws/aws-lambda-go/lambda"
)

func main() {
	pg := pagesJSON("main")
	s := startStat()
	os.Stderr.WriteString(pg + "\n")
	os.Stderr.WriteString("PROBE " + s + "\n")
	lambda.Start(func(context.Context) (string, error) { return s, nil })
}
