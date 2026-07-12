// threeclient: three AWS SDK v2 clients (DynamoDB, KMS, S3), all built at init
// and called per invoke. URL-connection HTTP client only (no Netty/Apache).

dependencies {
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
