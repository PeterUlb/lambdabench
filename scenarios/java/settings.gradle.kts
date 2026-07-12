/**
 * lambdabench Java Lambda artifacts: one subproject per scenario, each producing its
 * own deployment zip (via the `buildZip` task) so a scenario's artifact carries
 * only its own dependencies. The two Smithy scenarios additionally register the
 * smithy-base gradle plugin to generate a server SDK from the shared model.
 */

rootProject.name = "lambdabench-java"

pluginManagement {
    val smithyGradleVersion: String by settings
    plugins {
        id("software.amazon.smithy.gradle.smithy-base").version(smithyGradleVersion)
    }
    repositories {
        mavenCentral()
        gradlePluginPortal()
    }
}

include("hello")
include("oneclient")
include("threeclient")
include("lettercount")
include("batch")
include("cache")
include("authz")
include("smithy")
include("smithyfull")
