import * as cdk from "aws-cdk-lib";
import { Construct } from "constructs";
import * as s3 from "aws-cdk-lib/aws-s3";
import * as acm from "aws-cdk-lib/aws-certificatemanager";
import * as cloudfront from "aws-cdk-lib/aws-cloudfront";
import * as origins from "aws-cdk-lib/aws-cloudfront-origins";
import * as route53 from "aws-cdk-lib/aws-route53";
import * as targets from "aws-cdk-lib/aws-route53-targets";
import * as wafv2 from "aws-cdk-lib/aws-wafv2";

export interface SiteStackProps extends cdk.StackProps {
  /** Apex domain the site is served on, e.g. "bench.example.com". */
  readonly siteDomain: string;
  /** Id of the existing Route 53 hosted zone covering siteDomain. */
  readonly hostedZoneId: string;
  /** us-east-1 ACM certificate covering siteDomain. */
  readonly certificate: acm.Certificate;
  /** CLOUDFRONT-scoped WAF Web ACL to attach to the distribution. */
  readonly webAcl: wafv2.CfnWebACL;
}

/**
 * Long-lived static-hosting infrastructure for the LambdaBench site.
 *
 * The S3 bucket is private and reached only through CloudFront via Origin
 * Access Control (OAC), the private-origin pattern the flat-rate plan supports.
 * Apex domain, TLS certificate, and Web ACL are wired up at synth time, so a
 * `cdk deploy` never reconciles them away. The flat-rate plan subscription is the
 * only step still done via the CloudFront console (see deploy/README.md).
 */
export class SiteStack extends cdk.Stack {
  /** Private origin bucket; the publish step syncs the built site into it. */
  readonly bucket: s3.Bucket;
  /** Private archive bucket for raw benchmark results (run-* files) and the
   * documentation-probe outputs (lifecycle-*.json). Not fronted by CloudFront; the
   * runner archives the matrix run here right after it completes so an aborted
   * probe or publish cannot lose the hours-long run output. */
  readonly archiveBucket: s3.Bucket;
  /** The distribution serving the site; the publish step invalidates it. */
  readonly distribution: cloudfront.Distribution;

  constructor(scope: Construct, id: string, props: SiteStackProps) {
    super(scope, id, props);

    const zone = route53.HostedZone.fromHostedZoneAttributes(this, "Zone", {
      hostedZoneId: props.hostedZoneId,
      zoneName: props.siteDomain,
    });

    // Private origin: no public access, SSE-S3 at rest, versioned so a bad
    // publish can be rolled back. RETAIN so the data outlives a stack delete.
    this.bucket = new s3.Bucket(this, "SiteBucket", {
      blockPublicAccess: s3.BlockPublicAccess.BLOCK_ALL,
      encryption: s3.BucketEncryption.S3_MANAGED,
      enforceSSL: true,
      versioned: true,
      removalPolicy: cdk.RemovalPolicy.RETAIN,
    });

    // Private archive for raw benchmark output (run-*.jsonl.gz + run-*.meta.json)
    // plus the documentation-probe outputs (lifecycle-*-<id>.json). Same posture as
    // the site bucket, but no CloudFront origin: reachable only via authenticated
    // S3. The runner archives the matrix run here immediately after it completes
    // (before the probes) so a failed probe or publish does not lose it.
    this.archiveBucket = new s3.Bucket(this, "ArchiveBucket", {
      blockPublicAccess: s3.BlockPublicAccess.BLOCK_ALL,
      encryption: s3.BucketEncryption.S3_MANAGED,
      enforceSSL: true,
      versioned: true,
      removalPolicy: cdk.RemovalPolicy.RETAIN,
    });

    // No clean-URL edge rewrite is needed: the site is built with Observable's
    // `preserveExtension: true` (see site/observablehq.config.js), so every
    // internal link, canonical tag, and sitemap entry already points at a real
    // ".html" object. The root "/" is served via defaultRootObject.
    this.distribution = new cloudfront.Distribution(this, "SiteDistribution", {
      comment: "LambdaBench site",
      defaultRootObject: "index.html",
      httpVersion: cloudfront.HttpVersion.HTTP2_AND_3,
      // TLS_V1_2_2021 is the strongest profile compatible with broad viewer
      // coverage.
      minimumProtocolVersion: cloudfront.SecurityPolicyProtocol.TLS_V1_2_2021,
      // Standard access logs are intentionally not configured: the Free flat-rate
      // plan does not support them, and enabling them blocks the subscription. WAF
      // logs (covered by the plan) provide enough signal for the static site.
      defaultBehavior: {
        // OAC-backed private S3 origin: the construct emits the bucket policy
        // granting only this distribution s3:GetObject.
        origin: origins.S3BucketOrigin.withOriginAccessControl(this.bucket),
        viewerProtocolPolicy: cloudfront.ViewerProtocolPolicy.REDIRECT_TO_HTTPS,
        // Compress at the edge so the ~9 MB stats.json ships at its ~640 KB gzip size.
        compress: true,
        cachePolicy: cloudfront.CachePolicy.CACHING_OPTIMIZED,
      },
      domainNames: [props.siteDomain],
      certificate: props.certificate,
      webAclId: props.webAcl.attrArn,
    });

    // Route 53 ALIAS pointing the apex at the distribution. This record alone does
    // not enroll the zone in the flat-rate plan: the DNS benefit applies only once
    // the hosted zone is attached to the plan in the CloudFront console's "Manage
    // Plan" section (see deploy/README.md). Until then the zone stays on
    // pay-as-you-go pricing.
    new route53.ARecord(this, "SiteAlias", {
      zone,
      // Apex record: empty recordName targets the zone root.
      target: route53.RecordTarget.fromAlias(
        new targets.CloudFrontTarget(this.distribution),
      ),
    });

    new cdk.CfnOutput(this, "SiteBucketName", {
      value: this.bucket.bucketName,
    });
    new cdk.CfnOutput(this, "ArchiveBucketName", {
      value: this.archiveBucket.bucketName,
    });
    new cdk.CfnOutput(this, "DistributionId", {
      value: this.distribution.distributionId,
    });
    new cdk.CfnOutput(this, "DistributionDomainName", {
      value: this.distribution.distributionDomainName,
      description:
        "Default CloudFront domain; verify the site here before binding the apex domain via the plan.",
    });
  }
}
