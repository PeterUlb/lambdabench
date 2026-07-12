import * as cdk from "aws-cdk-lib";
import { Template, Match } from "aws-cdk-lib/assertions";
import { EdgeStack } from "../lib/edge-stack";
import { SiteStack } from "../lib/site-stack";

// Locks the security posture of the hosting layer. Both buckets (the CloudFront
// origin and the raw-results archive) are private, encrypted, versioned, and
// TLS-only, and the distribution terminates on strong TLS behind the WAF. A
// regression that loosened any of these (dropped a public-access block, removed
// SSE, weakened the min TLS version) would be silent in a `cdk deploy`, so each
// property is asserted explicitly rather than trusted to the construct defaults.
describe("SiteStack", () => {
  const app = new cdk.App();
  // EdgeStack supplies the cross-region cert + Web ACL SiteStack consumes.
  const edge = new EdgeStack(app, "TestEdgeStack", {
    env: { account: "111111111111", region: "us-east-1" },
    crossRegionReferences: true,
    siteDomain: "bench.example.com",
    hostedZoneId: "Z0123456789ABCDEFGHIJ",
  });
  const stack = new SiteStack(app, "TestSiteStack", {
    env: { account: "111111111111", region: "eu-central-1" },
    crossRegionReferences: true,
    siteDomain: "bench.example.com",
    hostedZoneId: "Z0123456789ABCDEFGHIJ",
    certificate: edge.certificate,
    webAcl: edge.webAcl,
  });
  const template = Template.fromStack(stack);

  test("exactly two buckets exist (site origin + archive)", () => {
    // Pins the count so a third, differently-configured bucket cannot slip in
    // unnoticed by the per-property assertions below (which match ALL buckets).
    template.resourceCountIs("AWS::S3::Bucket", 2);
  });

  test("every bucket blocks ALL public access", () => {
    // Both buckets must carry the full BLOCK_ALL configuration; asserting all
    // four flags (not just the block-* pair) rules out a partial relaxation.
    const blockAll = {
      BlockPublicAcls: true,
      BlockPublicPolicy: true,
      IgnorePublicAcls: true,
      RestrictPublicBuckets: true,
    };
    template.resourcePropertiesCountIs(
      "AWS::S3::Bucket",
      Match.objectLike({ PublicAccessBlockConfiguration: blockAll }),
      2,
    );
  });

  test("every bucket is encrypted at rest (SSE-S3)", () => {
    template.resourcePropertiesCountIs(
      "AWS::S3::Bucket",
      Match.objectLike({
        BucketEncryption: {
          ServerSideEncryptionConfiguration: [
            { ServerSideEncryptionByDefault: { SSEAlgorithm: "AES256" } },
          ],
        },
      }),
      2,
    );
  });

  test("every bucket is versioned", () => {
    template.resourcePropertiesCountIs(
      "AWS::S3::Bucket",
      Match.objectLike({ VersioningConfiguration: { Status: "Enabled" } }),
      2,
    );
  });

  test("every bucket denies non-TLS access (enforceSSL)", () => {
    // enforceSSL emits a bucket policy with an explicit Deny on
    // aws:SecureTransport=false; both buckets must carry it.
    template.resourcePropertiesCountIs(
      "AWS::S3::BucketPolicy",
      Match.objectLike({
        PolicyDocument: {
          Statement: Match.arrayWith([
            Match.objectLike({
              Effect: "Deny",
              Action: "s3:*",
              Condition: { Bool: { "aws:SecureTransport": "false" } },
            }),
          ]),
        },
      }),
      2,
    );
  });

  test("distribution redirects to HTTPS on strong TLS behind the WAF", () => {
    template.hasResourceProperties("AWS::CloudFront::Distribution", {
      DistributionConfig: Match.objectLike({
        // Viewers are forced onto HTTPS.
        DefaultCacheBehavior: Match.objectLike({
          ViewerProtocolPolicy: "redirect-to-https",
        }),
        // Min TLS pinned, not left to a weaker CloudFront default.
        ViewerCertificate: Match.objectLike({
          MinimumProtocolVersion: "TLSv1.2_2021",
        }),
        // Web ACL is actually attached.
        WebACLId: Match.anyValue(),
      }),
    });
  });

  test("the origin bucket is reached only via Origin Access Control (no public origin)", () => {
    // OAC is the private-origin mechanism; its presence (with an S3 origin that
    // has no public bucket policy) is what keeps the origin non-public.
    template.resourceCountIs("AWS::CloudFront::OriginAccessControl", 1);
  });
});
