plugins {
    java
    // Executes the smithy-build process to generate Rust server stubs.
    id("software.amazon.smithy.gradle.smithy-base")
}

description = "LambdaBench CoffeeShop Rust server codegen."

dependencies {
    val smithyRsVersion: String by project

    // Code generator
    smithyBuild("software.amazon.smithy.rust:codegen-server:$smithyRsVersion")

    // Service model
    implementation(project(":smithy"))
}
