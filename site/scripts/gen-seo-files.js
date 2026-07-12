// Post-build step: emit sitemap.xml and robots.txt into the build output, and
// patch the generated HTML with a document language.
//
// Framework only writes page paths and referenced files to the output dir;
// standalone files like these are not copied, and `rel="external"` head links do
// not trigger a copy (framework discussion #1996). The endorsed approach is a
// build step that reads the config and writes the files directly.
import { copyFile, readFile, readdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import config, { SITE_URL, servedPath } from "../observablehq.config.js";

const OUTPUT_DIR = join(import.meta.dirname, "..", config.output ?? "dist");
const SRC_DIR = join(import.meta.dirname, "..", config.root ?? "src");

// Document language for the <html> element. The site is English-only.
const SITE_LANG = "en";

// Icons that browsers and crawlers probe at fixed root paths regardless of any
// <link> tag (iOS requests /apple-touch-icon.png; crawlers request
// /favicon.svg). Framework emits these only under hashed /_file/ paths, so copy
// the originals to the output root for the bare-root convention.
const ROOT_ICONS = ["favicon.svg", "apple-touch-icon.png"];

// Map a configured page path to its absolute canonical URL. servedPath applies
// the ".html" suffix that `preserveExtension` uses for the actual objects, so
// the sitemap URLs resolve without a clean-URL edge rewrite.
function canonical(path) {
  return `${SITE_URL}${servedPath(path)}`;
}

const urls = (config.pages ?? [])
  .filter((p) => p.path && !/^\w+:/.test(p.path))
  .map((p) => canonical(p.path));

const sitemap = `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
${urls.map((u) => `  <url><loc>${u}</loc></url>`).join("\n")}
</urlset>
`;

const robots = `User-agent: *
Allow: /
Sitemap: ${SITE_URL}/sitemap.xml
`;

await writeFile(join(OUTPUT_DIR, "sitemap.xml"), sitemap);
await writeFile(join(OUTPUT_DIR, "robots.txt"), robots);

// Framework 1.13.4 hardcodes a bare `<html>` tag with no config hook for the
// document language, so add lang here: walk the output tree and rewrite the
// first bare `<html>` on every generated page.
async function patchHtmlLang(dir) {
  let patched = 0;
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      patched += await patchHtmlLang(full);
    } else if (entry.name.endsWith(".html")) {
      const html = await readFile(full, "utf8");
      if (html.includes("<html>")) {
        await writeFile(
          full,
          html.replace("<html>", `<html lang="${SITE_LANG}">`),
        );
        patched++;
      }
    }
  }
  return patched;
}

const patched = await patchHtmlLang(OUTPUT_DIR);

// Fail loud if the patch matched nothing: a Framework upgrade that changes the
// `<html>` tag would otherwise silently ship every page with no lang attribute.
if (patched === 0) {
  throw new Error(
    'patchHtmlLang matched no pages: Framework\'s "<html>" tag likely changed. Update the replace in gen-seo-files.js.',
  );
}

// Copy the icon originals to the output root for the bare-path convention.
for (const icon of ROOT_ICONS) {
  await copyFile(join(SRC_DIR, icon), join(OUTPUT_DIR, icon));
}

console.log(
  `wrote sitemap.xml (${urls.length} urls) and robots.txt, set lang="${SITE_LANG}" on ${patched} pages, ` +
    `copied ${ROOT_ICONS.length} root icons, in ${OUTPUT_DIR}`,
);
