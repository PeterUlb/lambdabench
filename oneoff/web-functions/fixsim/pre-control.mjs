// Control preload: same NODE_OPTIONS=--import mechanism as pre-fix.mjs, changes nothing.
globalThis.__fixsim = { variant: "control", preloadMs: +performance.now().toFixed(1) };
