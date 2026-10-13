package main

import (
	"fmt"
	"os"
	"strings"
	"syscall"
)

// startStat describes the process at the first line of main: major faults,
// bytes read from disk, how much of the executable is resident, and the size
// of each virtio disk.
func startStat() string {
	var ru syscall.Rusage
	_ = syscall.Getrusage(syscall.RUSAGE_SELF, &ru)
	readKB := int64(-1)
	if b, err := os.ReadFile("/proc/self/io"); err == nil {
		for _, line := range strings.Split(string(b), "\n") {
			var v int64
			if _, err := fmt.Sscanf(line, "read_bytes: %d", &v); err == nil {
				readKB = v / 1024
			}
		}
	}
	exe, _ := os.Readlink("/proc/self/exe")
	var exeRss, exeSize, otherFileRss int64
	if b, err := os.ReadFile("/proc/self/smaps"); err == nil {
		cur := ""
		for _, line := range strings.Split(string(b), "\n") {
			f := strings.Fields(line)
			if len(f) >= 5 && strings.Contains(f[0], "-") {
				cur = ""
				if len(f) >= 6 {
					cur = f[5]
				}
				continue
			}
			if len(f) >= 2 && (f[0] == "Rss:" || f[0] == "Size:") {
				var v int64
				fmt.Sscanf(f[1], "%d", &v)
				switch {
				case cur == exe && f[0] == "Rss:":
					exeRss += v
				case cur == exe:
					exeSize += v
				case strings.HasPrefix(cur, "/") && f[0] == "Rss:":
					otherFileRss += v
				}
			}
		}
	}
	parts := ""
	if b, err := os.ReadFile("/proc/partitions"); err == nil {
		for _, line := range strings.Split(string(b), "\n") {
			f := strings.Fields(line)
			if len(f) == 4 && strings.HasPrefix(f[3], "vd") {
				parts += f[3] + "=" + f[2] + "KB "
			}
		}
	}
	fi, _ := os.Stat(exe)
	return fmt.Sprintf(`{"start_majflt":%d,"start_minflt":%d,"start_read_kb":%d,"exe":%q,"exe_file_kb":%d,"exe_map_kb":%d,"exe_rss_kb":%d,"other_file_rss_kb":%d,"partitions":%q}`,
		ru.Majflt, ru.Minflt, readKB, exe, fi.Size()/1024, exeSize, exeRss, otherFileRss, strings.TrimSpace(parts))
}
