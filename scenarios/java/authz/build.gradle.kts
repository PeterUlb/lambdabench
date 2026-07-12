// authz: the realistic JWT-authorizer hot path. Verifies an RS256 JWT (native
// JCA crypto via nimbus-jose-jwt, the de-facto JVM JOSE library) and extracts
// claims. No AWS clients: the token arrives in the invoke payload and the public
// verification key is embedded as a resource.

dependencies {
    implementation("com.nimbusds:nimbus-jose-jwt:9.40")
    implementation("com.fasterxml.jackson.core:jackson-databind:2.18.2")
}

// The public JWK is a build-time fixture generated (idempotently) by the shared
// bencher generator: the SAME fixture the Rust and Node authz handlers embed.
// It is gitignored (it pairs with a private key), so generate it before
// packaging resources and copy it into the build resources dir.
val fixturesDir = file("$rootDir/../../bencher/fixtures")
val generateAuthzFixture = tasks.register<Exec>("generateAuthzFixture") {
    workingDir = rootDir
    commandLine("node", "$fixturesDir/generate.mjs")
    // generate.mjs is idempotent; declare the outputs so up-to-date checks skip
    // regeneration once they exist.
    outputs.file("$fixturesDir/authz_public_jwk.json")
    outputs.file("$fixturesDir/authz_token.txt")
}

tasks.named<ProcessResources>("processResources") {
    dependsOn(generateAuthzFixture)
    // Bundle the public JWK (verification key) and, for SnapStart priming, the
    // signed token fixture so the beforeCheckpoint hook can run a real verify.
    from(fixturesDir) {
        include("authz_public_jwk.json")
        include("authz_token.txt")
    }
}
