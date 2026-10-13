package cpuinfo

import (
	"context"
	"testing"

	"github.com/aws/aws-lambda-go/lambdacontext"
)

func TestDescribe(t *testing.T) {
	arm := "processor\t: 0\nCPU implementer\t: 0x41\nCPU variant\t: 0x1\nCPU part\t: 0xd40\n\nprocessor\t: 1\nCPU implementer\t: 0x41\nCPU variant\t: 0x1\nCPU part\t: 0xd40\n"
	if got, want := describe(arm), "arm implementer=0x41 part=0xd40 variant=0x1; vcpus=2"; got != want {
		t.Errorf("arm64: got %q, want %q", got, want)
	}
	x86 := "processor\t: 0\ncpu family\t: 6\nmodel\t\t: 63\nmodel name\t: Intel(R) Xeon(R) Processor @ 2.50GHz\nstepping\t: 2\n\nprocessor\t: 1\nmodel name\t: Intel(R) Xeon(R) Processor @ 2.50GHz\n"
	if got, want := describe(x86), "Intel(R) Xeon(R) Processor @ 2.50GHz; vcpus=2"; got != want {
		t.Errorf("x86: got %q, want %q", got, want)
	}
}

func TestRequestedOnlyOnTheFlag(t *testing.T) {
	if requested(context.Background()) {
		t.Error("no Lambda context: want false")
	}
	with := func(custom map[string]string) context.Context {
		lc := &lambdacontext.LambdaContext{}
		lc.ClientContext.Custom = custom
		return lambdacontext.NewContext(context.Background(), lc)
	}
	if requested(with(nil)) {
		t.Error("no ClientContext: want false")
	}
	if requested(with(map[string]string{"x": "cpu"})) {
		t.Error("other key: want false")
	}
	if !requested(with(map[string]string{"lambdabench": "cpu"})) {
		t.Error("flag set: want true")
	}
}
