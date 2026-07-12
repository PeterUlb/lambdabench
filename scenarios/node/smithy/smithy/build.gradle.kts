description = "LambdaBench CoffeeShop model (shared) for TypeScript server codegen."

plugins {
    `java-library`
    id("software.amazon.smithy.gradle.smithy-jar")
}

dependencies {
    val smithyVersion: String by project
    implementation("software.amazon.smithy:smithy-model:$smithyVersion")
    implementation("software.amazon.smithy:smithy-aws-traits:$smithyVersion")
    implementation("software.amazon.smithy:smithy-validation-model:$smithyVersion")
    implementation("software.amazon.smithy.typescript:smithy-aws-typescript-codegen:0.22.0")
}

// Single shared model as the source of truth.
sourceSets {
    main {
        smithy {
            srcDir("$rootDir/../../../smithy/model")
        }
    }
}
