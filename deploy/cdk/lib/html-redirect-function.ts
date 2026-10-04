// Viewer-request CloudFront Function: 301 "/comparison" and "/comparison/" to
// "/comparison.html". The site's own links already end in ".html"
// (`preserveExtension`); this catches external links that drop it, which would
// otherwise 403 on the private origin and lose the backlink. A 301, not a
// rewrite, keeps one URL per page. The query string is dropped (unused).
//
// A string in a .ts module because this package gitignores *.js (tsc output).
// cloudfront-js-2.0 runtime: a bare `handler`, no module syntax.
export const HTML_REDIRECT_FUNCTION_CODE = `
function handler(event) {
  var request = event.request;
  var uri = request.uri;
  var last = uri.substring(uri.lastIndexOf("/") + 1);
  // The root (defaultRootObject) and any path with an extension pass through.
  if (uri === "/" || last.indexOf(".") !== -1) return request;
  var base = uri.replace(/\\/+$/, "");
  if (base === "") return request;
  return {
    statusCode: 301,
    statusDescription: "Moved Permanently",
    headers: { location: { value: base + ".html" } },
  };
}
`;
