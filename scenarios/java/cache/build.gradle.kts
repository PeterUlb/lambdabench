// cache: the dedicated retained-heap GC probe. Allocates a large (~100 MB) live
// set at init, held across warm invokes, and churns a fraction of it each invoke.
// No AWS clients, no framework, no JSON. Inherits the Java 25 toolchain,
// aws-lambda-java-core, the org.crac dependency, and the buildZip task from the
// root build script. It registers NO beforeCheckpoint hook: cache is a pure-CPU
// GC probe with no SDK path an operator could hoist, so it is left genuinely
// unprimed (see DESIGN.md §10).
