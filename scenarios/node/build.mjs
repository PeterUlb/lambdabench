// Bundles each Node scenario into a single minified ESM file with esbuild,
// targeting the nodejs24.x runtime. The AWS SDK is bundled (not externalized)
// so the Node functions ship their own SDK, matching Rust's static linking for
// a fair comparison.
//
// Output: dist/<scenario>/index.mjs  (the bencher zips these for deployment).

import { build } from "esbuild";
import { mkdir, rm, copyFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const scenarios = ["hello", "smithy", "oneclient", "threeclient", "smithyfull", "lettercount", "authz", "batch", "cache"];

// The Smithy SSDK validates @pattern/@length constraints via re2-wasm, whose
// emscripten glue loads `re2.wasm` from the bundle directory at runtime. Copy
// it next to the bundle so it ends up in the deployment zip.
async function copyRe2Wasm(outdir) {
  const wasm = join(dirname(require.resolve("re2-wasm/package.json")), "build", "wasm", "re2.wasm");
  if (!existsSync(wasm)) {
    throw new Error(`re2.wasm not found at ${wasm}`);
  }
  await copyFile(wasm, join(outdir, "re2.wasm"));
}

// Ensure the authz crypto fixtures exist before bundling: the authz handler
// imports the generated public JWK. The generator is idempotent (no-op if the
// files already exist). Kept out of git (see bencher/fixtures/generate.mjs).
await import(join(here, "..", "..", "bencher", "fixtures", "generate.mjs"));

await rm(join(here, "dist"), { recursive: true, force: true });

for (const scenario of scenarios) {
  const outdir = join(here, "dist", scenario);
  await mkdir(outdir, { recursive: true });
  await build({
    entryPoints: [join(here, scenario, "index.mjs")],
    outfile: join(outdir, "index.mjs"),
    bundle: true,
    minify: true,
    platform: "node",
    target: "node24",
    format: "esm",
    // Node 24 ESM bundles: shim require()/__dirname/__filename for any
    // transitive CJS dependency that expects the CommonJS globals.
    banner: {
      js: [
        "import { createRequire as __cr } from 'module';",
        "import { fileURLToPath as __ftp } from 'url';",
        "import { dirname as __dn } from 'path';",
        "const require = __cr(import.meta.url);",
        "const __filename = __ftp(import.meta.url);",
        "const __dirname = __dn(__filename);",
      ].join(""),
    },
    logLevel: "info",
  });
  // Both Smithy-hosted scenarios use the SSDK, which needs re2.wasm at runtime.
  if (scenario === "smithy" || scenario === "smithyfull") {
    await copyRe2Wasm(outdir);
  }
  console.log(`built node:${scenario} -> ${outdir}/index.mjs`);
}
