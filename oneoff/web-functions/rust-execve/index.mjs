// Entry module (variant "index"): runs after AWS's wrapper has loaded and fetched the
// configuration, then replaces this process with the Rust server, as in
// https://awsteele.com/blog/2026/10/12/go-on-lambda-web-functions-via-execve.html
process.execve("/var/task/server", ["/var/task/server"], {
  ...process.env,
  NODE_TIME_ORIGIN: String(performance.timeOrigin),
  EXECVE_AT_MS: performance.now().toFixed(1),
});
