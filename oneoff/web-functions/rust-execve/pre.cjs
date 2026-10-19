// NODE_OPTIONS=--require preload (variant "preload"): runs right after Node's bootstrap, before
// AWS's wrapper, and replaces the process with the Rust server, which then fetches the
// configuration (/load) and reports readiness itself. CommonJS, so Node's ES module loader never
// starts.
const env = { ...process.env, NODE_TIME_ORIGIN: String(performance.timeOrigin), EXECVE_AT_MS: performance.now().toFixed(1), CALL_LOAD: "1" };
delete env.NODE_OPTIONS;
process.execve("/var/task/server", ["/var/task/server"], env);
