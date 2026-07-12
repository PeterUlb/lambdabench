/**
 * Shared configuration for every lambdabench Java scenario subproject.
 *
 * Each subproject is a `java-library` targeting Java 25 (the `java25` Lambda
 * runtime) that declares only the AWS Lambda core types plus whatever that
 * scenario needs (an AWS SDK v2 client, a JWT library, or the smithy-java
 * server stack). The shared `buildZip` task packages the compiled classes and
 * the full runtime classpath under `lib/`, the layout the Java managed runtime
 * expects on a deployment zip.
 */

subprojects {
    apply(plugin = "java-library")

    repositories {
        mavenCentral()
    }

    extensions.configure<JavaPluginExtension> {
        toolchain {
            languageVersion.set(JavaLanguageVersion.of(25))
        }
    }

    dependencies {
        // The Lambda core types (RequestHandler, Context) are needed by every
        // handler, including the smithy-java LambdaEndpoint.
        "implementation"("com.amazonaws:aws-lambda-java-core:1.2.3")
        // CRaC runtime hooks (org.crac), used to PRIME SnapStart snapshots: a
        // beforeCheckpoint() hook runs one representative invocation during init
        // so the SDK class-loading / marshaller construction / JIT cost is baked
        // into the snapshot rather than paid on the first restored invoke. The
        // hook fires ONLY when a checkpoint is taken (SnapStart on); on a plain
        // function org.crac uses a no-op context, so the same jar is unaffected.
        "implementation"("org.crac:crac:1.4.0")
    }

    // Package a Lambda deployment zip: compiled jar + runtime dependencies under
    // lib/. The bencher's build step copies <subproject>/build/distributions/*.zip
    // into dist/java-<scenario>.zip.
    tasks.register<Zip>("buildZip") {
        val jarTask = tasks.named("jar")
        into("lib") {
            from(jarTask)
            from(configurations.named("runtimeClasspath"))
        }
    }
}
