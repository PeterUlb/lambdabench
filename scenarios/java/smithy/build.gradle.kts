// smithy: the Smithy server framework hosted behind a Lambda handler, with NO
// AWS call. Generates a CoffeeShop server SDK from the shared model via
// smithy-java's java-codegen (server mode) and serves GetMenu behind the
// smithy-java LambdaEndpoint. The cold-start delta versus `hello` isolates the
// Smithy server framework overhead.

plugins {
    id("software.amazon.smithy.gradle.smithy-base")
}

val smithyJavaVersion: String by project
val smithyVersion: String by project

dependencies {
    // SPI registration for the SmithyServiceProvider the LambdaEndpoint loads.
    annotationProcessor("com.google.auto.service:auto-service:1.1.1")
    compileOnly("com.google.auto.service:auto-service:1.1.1")

    // Model traits the shared model uses, on the build classpath so codegen can
    // resolve them: restJson1 (smithy-aws-traits) and ValidationException
    // (smithy-validation-model).
    smithyBuild("software.amazon.smithy:smithy-aws-traits:$smithyVersion")
    smithyBuild("software.amazon.smithy:smithy-validation-model:$smithyVersion")

    // Codegen plugins run at build time on the smithyBuild classpath.
    smithyBuild("software.amazon.smithy.java:codegen-plugin:$smithyJavaVersion")
    smithyBuild("software.amazon.smithy.java:server-api:$smithyJavaVersion")

    // Runtime: the Lambda glue, the generated server runtime, and the protocols
    // the service is annotated with (restJson1 + rpcv2Cbor).
    implementation("software.amazon.smithy.java:aws-lambda-endpoint:$smithyJavaVersion")
    implementation("software.amazon.smithy.java:server-api:$smithyJavaVersion")
    implementation("software.amazon.smithy.java:aws-server-restjson:$smithyJavaVersion")
    implementation("software.amazon.smithy.java:server-rpcv2-cbor:$smithyJavaVersion")
}

// The shared model is the single source of truth, same one the Rust/Node smithy
// projects generate from. It is registered on the `smithy` source set (the
// smithy-base plugin discovers models there), not the `java` one.
sourceSets {
    main {
        smithy {
            srcDir("$rootDir/../../smithy/model")
        }
    }
}

// Wire the java-codegen projection output into the main source set so the
// generated server classes compile alongside the handwritten provider.
afterEvaluate {
    val serverPath = smithy.getPluginProjectionPath(smithy.sourceProjection.get(), "java-codegen").get()
    sourceSets {
        main {
            java {
                srcDir("$serverPath/java")
            }
        }
    }
}

tasks.named("compileJava") {
    dependsOn(tasks.named("smithyBuild"))
}

repositories {
    mavenCentral()
}
