import json, sys
secfile, pagefile = sys.argv[1], sys.argv[2]
secs = []
for line in open(secfile):
    n, off, size = line.split()
    off, size = int(off, 16), int(size, 16)
    if n in ('.bss', '.noptrbss', '.shstrtab') or size == 0:
        continue
    secs.append((n, off, size))
def bits(h, n):
    b = bytes.fromhex(h)
    return [(b[i // 8] >> (i % 8)) & 1 for i in range(n)]
rows = [json.loads(l.split(' ', 2)[2]) for l in open(pagefile)]
print(f"{len(rows)} samples")
r = rows[0]
n = r['pages']
for key in ('cached', 'mapped'):
    allv = [bits(x[key], n) for x in rows]
    v = [round(sum(col) / len(col)) for col in zip(*allv)]  # majority across samples
    tot = sum(v)
    print(f"{key}: {tot * 4} KB of {n * 4} KB ({100 * tot / n:.0f}%)")
    for name, off, size in secs:
        a, b = off // 4096, (off + size + 4095) // 4096
        c = sum(v[a:b])
        # how many 128 KB-aligned (32-page) chunks of the section have any page set
        chunks = {p // 32 for p in range(a, b) if v[p]}
        allchunks = {p // 32 for p in range(a, b)}
        print(f"  {name:18} {size // 1024:6d} KB  {key} {c * 4:6d} KB ({100 * c / max(1, b - a):3.0f}%)  "
              f"128K-chunks touched {len(chunks)}/{len(allchunks)}")
