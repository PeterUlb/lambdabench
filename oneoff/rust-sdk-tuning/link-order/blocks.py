import sys, subprocess
binp = sys.argv[1]
reqonly = set(l.strip() for l in open('sf-order/reqonly.txt') if l.strip())
order2 = [l.strip() for l in open('sf-order/order2.txt') if l.strip()]
init = set(order2) - reqonly
ph = subprocess.run(['readelf', '-lW', binp], capture_output=True, text=True).stdout
segs = [(int(f[2], 16), int(f[2], 16) + int(f[4], 16), int(f[1], 16)) for f in (l.split() for l in ph.splitlines()) if f and f[0] == 'LOAD']
def fo(va):
    for a, b, o in segs:
        if a <= va < b: return va - a + o
ib, rb = set(), set(); rsize = 0
for l in subprocess.run(['nm', '-S', '--defined-only', binp], capture_output=True, text=True).stdout.splitlines():
    f = l.split()
    if len(f) != 4 or f[2] not in 'tT': continue
    o = fo(int(f[0], 16)); sz = int(f[1], 16)
    if o is None: continue
    rng = set(range(o // 131072, (o + max(sz, 1) - 1) // 131072 + 1))
    if f[3] in init: ib |= rng
    elif f[3] in reqonly: rb |= rng; rsize += sz
print(f"{binp.split('/')[-1]}: init code in {len(ib)} blocks; request-only code {rsize//1024} KB in {len(rb)} blocks, {len(rb - ib)} of them not touched by init code")
