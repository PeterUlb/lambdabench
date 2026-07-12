// batch: deserialize-heavy batch record processor. Fetches a ~16 MB JSON record array
// from S3 once at init, then per warm invoke parses the whole batch and
// groups-by key into a map. Needs the S3 client and a JSON parser.

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
