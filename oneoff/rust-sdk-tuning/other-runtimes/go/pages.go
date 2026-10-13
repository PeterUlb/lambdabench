package main

import (
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"os"
	"strconv"
	"strings"
	"syscall"
	"unsafe"
)

// exePages returns two bitmaps over the executable's 4 KB file pages: which
// pages are in the page cache (mincore on a fresh read-only mapping, which
// does not fault anything in) and which are mapped into this process
// (present bits in /proc/self/pagemap for the executable's own mappings).
func exePages() (cached, mapped string, pages int) {
	exe, _ := os.Readlink("/proc/self/exe")
	f, err := os.Open(exe)
	if err != nil {
		return "", "", 0
	}
	defer f.Close()
	fi, _ := f.Stat()
	size := int(fi.Size())
	pages = (size + 4095) / 4096
	cbits := make([]byte, (pages+7)/8)
	mbits := make([]byte, (pages+7)/8)

	if m, err := syscall.Mmap(int(f.Fd()), 0, size, syscall.PROT_READ, syscall.MAP_SHARED); err == nil {
		vec := make([]byte, pages)
		_, _, e := syscall.Syscall(syscall.SYS_MINCORE, uintptr(unsafe.Pointer(&m[0])), uintptr(size), uintptr(unsafe.Pointer(&vec[0])))
		if e == 0 {
			for i, v := range vec {
				if v&1 != 0 {
					cbits[i/8] |= 1 << (i % 8)
				}
			}
		}
		_ = syscall.Munmap(m)
	}

	maps, _ := os.ReadFile("/proc/self/maps")
	pm, err := os.Open("/proc/self/pagemap")
	if err == nil {
		defer pm.Close()
		buf := make([]byte, 8)
		for _, line := range strings.Split(string(maps), "\n") {
			fl := strings.Fields(line)
			if len(fl) < 6 || fl[5] != exe {
				continue
			}
			r := strings.SplitN(fl[0], "-", 2)
			start, _ := strconv.ParseUint(r[0], 16, 64)
			end, _ := strconv.ParseUint(r[1], 16, 64)
			off, _ := strconv.ParseUint(fl[2], 16, 64)
			for va := start; va < end; va += 4096 {
				if _, err := pm.ReadAt(buf, int64(va/4096*8)); err != nil {
					break
				}
				if binary.LittleEndian.Uint64(buf)>>63 == 1 {
					p := int((off + va - start) / 4096)
					if p < pages {
						mbits[p/8] |= 1 << (p % 8)
					}
				}
			}
		}
	}
	return hex.EncodeToString(cbits), hex.EncodeToString(mbits), pages
}

func pagesJSON(tag string) string {
	c, m, n := exePages()
	return fmt.Sprintf(`PAGES %s {"pages":%d,"cached":%q,"mapped":%q}`, tag, n, c, m)
}
