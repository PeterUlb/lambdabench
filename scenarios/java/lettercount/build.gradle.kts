// lettercount: CPU-bound work with no per-invoke I/O. Fetches a ~1 MB JSON
// string array from S3 once at init, then counts a-z per warm invoke. Needs the
// S3 client (init fetch) and a JSON parser (the per-invoke allocation source).

dependencies {
    implementation(platform("software.amazon.awssdk:bom:2.31.6"))
    implementation("software.amazon.awssdk:s3")
    implementation("software.amazon.awssdk:url-connection-client")
    implementation("com.fasterxml.jackson.core:jackson-databind:2.18.2")
}

configurations.all {
    exclude(group = "software.amazon.awssdk", module = "apache-client")
    exclude(group = "software.amazon.awssdk", module = "netty-nio-client")
}
