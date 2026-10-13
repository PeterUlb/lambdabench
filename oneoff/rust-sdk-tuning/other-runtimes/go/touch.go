package main

import (
	"encoding/binary"
	"fmt"
	"os"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"unsafe"
)

type exeMap struct{ start, end, off uint64 }

func exeMaps() []exeMap {
	exe, _ := os.Readlink("/proc/self/exe")
	b, _ := os.ReadFile("/proc/self/maps")
	var out []exeMap
	for _, line := range strings.Split(string(b), "\n") {
		f := strings.Fields(line)
		if len(f) < 6 || f[5] != exe {
			continue
		}
		r := strings.SplitN(f[0], "-", 2)
		s, _ := strconv.ParseUint(r[0], 16, 64)
		e, _ := strconv.ParseUint(r[1], 16, 64)
		o, _ := strconv.ParseUint(f[2], 16, 64)
		out = append(out, exeMap{s, e, o})
	}
	return out
}

func threadIOReadKB() float64 {
	b, err := os.ReadFile("/proc/thread-self/io")
	if err != nil {
		return -1
	}
	for _, line := range strings.Split(string(b), "\n") {
		if v, ok := strings.CutPrefix(line, "read_bytes: "); ok {
			n, _ := strconv.ParseFloat(v, 64)
			return n / 1024
		}
	}
	return -1
}

func threadUsage() (cpuMS float64, maj int64) {
	var ru syscall.Rusage
	_ = syscall.Getrusage(1 /* RUSAGE_THREAD */, &ru)
	return float64(ru.Utime.Nano()+ru.Stime.Nano()) / 1e6, ru.Majflt
}

// touchBlocks reads one byte at the start of up to n file-aligned 128 KB
// blocks of the executable that have no page mapped (pagemap) in themselves
// or their neighbours, and reports CPU, major faults and KB read per touch.
func touchBlocks(n int) string {
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	pm, err := os.Open("/proc/self/pagemap")
	if err != nil {
		return `{"error":"pagemap"}`
	}
	defer pm.Close()
	buf := make([]byte, 8)
	present := func(va uint64) bool {
		if _, err := pm.ReadAt(buf, int64(va/4096*8)); err != nil {
			return true
		}
		return binary.LittleEndian.Uint64(buf)>>63 == 1
	}
	const blk = 128 * 1024
	var out []string
	for _, m := range exeMaps() {
		// File-aligned 128 KB blocks fully inside this mapping.
		first := (m.off + blk - 1) / blk * blk
		for fo := first + blk; fo+2*blk <= m.off+(m.end-m.start) && len(out) < n; fo += 3 * blk {
			va := m.start + (fo - m.off)
			clean := true
			for p := va - blk; p < va+2*blk; p += 4096 {
				if present(p) {
					clean = false
					break
				}
			}
			if !clean {
				continue
			}
			c0, m0 := threadUsage()
			r0 := threadIOReadKB()
			_ = *(*byte)(unsafe.Pointer(uintptr(va)))
			c1, m1 := threadUsage()
			r1 := threadIOReadKB()
			out = append(out, fmt.Sprintf(`{"off_kb":%d,"cpu":%.3f,"majflt":%d,"read_kb":%.0f}`, fo/1024, c1-c0, m1-m0, r1-r0))
		}
	}
	return "[" + strings.Join(out, ",") + "]"
}
