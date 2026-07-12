description = "LambdaBench CoffeeShop model (shared) for Rust server codegen."
plugins {
    `java-library`
    id("software.amazon.smithy.gradle.smithy-jar")
}
dependencies {
    val smithyVersion: String by project
    api("software.amazon.smithy:smithy-aws-traits:$smithyVersion")
    api("software.amazon.smithy:smithy-validation-model:$smithyVersion")
}
smithy {
    sourceProjection.set("source")
}
sourceSets {
    main {
        smithy {
            srcDir("$rootDir/../../../smithy/model")
        }
    }
}
