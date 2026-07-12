// oneclient: one AWS SDK v2 client (DynamoDB), constructed at init and called
// once per invoke. Uses the URL-connection HTTP client (no Netty/Apache) for a
// lean cold start, matching the "construct + call one client" shape.

dependencies {
    implementation(platform("software.amazon.awssdk:bom:2.31.6"))
    implementation("software.amazon.awssdk:dynamodb")
    implementation("software.amazon.awssdk:url-connection-client")
}

// Drop the default HTTP clients so only the URL-connection client ships.
configurations.all {
    exclude(group = "software.amazon.awssdk", module = "apache-client")
    exclude(group = "software.amazon.awssdk", module = "netty-nio-client")
}
