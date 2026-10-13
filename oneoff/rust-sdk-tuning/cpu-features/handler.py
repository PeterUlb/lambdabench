"""CPU feature probe: on each cold start, reports /proc/cpuinfo (model, cache, flags,
kernel bug list) and times SHA-256/SHA-512/SHA3-256 over 1 MiB buffers and a CPU-bound Python
loop. See ../README.md, "Host CPU"."""
import hashlib, time, os

# True until the first invoke of this sandbox, so a result can prove it was a cold start.
_COLD = True

def _cpu():
    blocks = [b for b in open("/proc/cpuinfo").read().split("\n\n") if b.strip()]
    out = {}
    for line in blocks[0].splitlines():
        k, _, v = line.partition(":")
        out[k.strip()] = v.strip()
    flags = sorted(set((out.get("flags", "") + " " + out.get("Features", "")).split()))
    return {
        "model_name": out.get("model name"), "family": out.get("cpu family"), "model": out.get("model"),
        "stepping": out.get("stepping"), "microcode": out.get("microcode"),
        "implementer": out.get("CPU implementer"), "part": out.get("CPU part"), "variant": out.get("CPU variant"),
        "revision": out.get("CPU revision"), "mhz": out.get("cpu MHz"), "bogomips": out.get("bogomips") or out.get("BogoMIPS"),
        "cache": out.get("cache size"), "vcpus": len(blocks), "flags": flags, "bugs": out.get("bugs"),
    }

def _t(fn):
    t = time.process_time(); fn(); return round((time.process_time() - t) * 1000, 2)

def handler(event, context):
    global _COLD
    cold, _COLD = _COLD, False
    data = b"x" * (1 << 20)
    r = _cpu()
    r["cold"] = cold
    r["sha256_ms"] = _t(lambda: [hashlib.sha256(data).digest() for _ in range(40)])
    r["sha512_ms"] = _t(lambda: [hashlib.sha512(data).digest() for _ in range(40)])
    r["sha3_ms"] = _t(lambda: [hashlib.sha3_256(data).digest() for _ in range(20)])
    def loop():
        s = 0
        for i in range(2_000_000):
            s += i * i % 7
    r["py_ms"] = _t(loop)
    r["openssl"] = __import__("ssl").OPENSSL_VERSION
    return r
