// smithyfull: the realistic shape, the Smithy server framework hosting a
// handler that does a real AWS write flow. CreateOrder KMS-encrypts a signature,
// DDB PutItems the order, and S3 PutObjects a receipt, then returns a
// constraint-validated Uuid response. Same generated CoffeeShop server SDK as
// the smithy scenario, plus the three AWS SDK v2 clients.

plugins {
    id("software.amazon.smithy.gradle.smithy-base")
}

val smithyJavaVersion: String by project
val smithyVersion: String by project

dependencies {
    annotationProcessor("com.google.auto.service:auto-service:1.1.1")
    compileOnly("com.google.auto.service:auto-service:1.1.1")

    smithyBuild("software.amazon.smithy:smithy-aws-traits:$smithyVersion")
    smithyBuild("software.amazon.smithy:smithy-validation-model:$smithyVersion")
    smithyBuild("software.amazon.smithy.java:codegen-plugin:$smithyJavaVersion")
    smithyBuild("software.amazon.smithy.java:server-api:$smithyJavaVersion")

    implementation("software.amazon.smithy.java:aws-lambda-endpoint:$smithyJavaVersion")
    implementation("software.amazon.smithy.java:server-api:$smithyJavaVersion")
    implementation("software.amazon.smithy.java:aws-server-restjson:$smithyJavaVersion")
    implementation("software.amazon.smithy.java:server-rpcv2-cbor:$smithyJavaVersion")

    // The realistic write flow: three AWS SDK v2 clients (URL-connection client
    // only, no Netty/Apache).
    implementation(platform("software.amazon.awssdk:bom:2.31.6"))
    implementation("software.amazon.awssdk:dynamodb")
    implementation("software.amazon.awssdk:kms")
    implementation("software.amazon.awssdk:s3")
    implementation("software.amazon.awssdk:url-connection-client")
}

configurations.all {
    exclude(group = "software.amazon.awssdk", module = "apache-client")
    exclude(group = "software.amazon.awssdk", module = "netty-nio-client")
}

sourceSets {
    main {
        smithy {
            srcDir("$rootDir/../../smithy/model")
        }
    }
}

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
