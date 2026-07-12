import * as cdk from "aws-cdk-lib";
import { Construct } from "constructs";
import * as acm from "aws-cdk-lib/aws-certificatemanager";
import * as route53 from "aws-cdk-lib/aws-route53";
import * as wafv2 from "aws-cdk-lib/aws-wafv2";

export interface EdgeStackProps extends cdk.StackProps {
  /** Apex domain the site is served on, e.g. "bench.example.com". */
  readonly siteDomain: string;
  /** Id of the existing Route 53 hosted zone covering siteDomain. */
  readonly hostedZoneId: string;
}

/**
 * Holds the CloudFront-scoped resources that must live in us-east-1: the ACM
 * certificate (CloudFront only consumes us-east-1 certs) and the CLOUDFRONT-
 * scoped WAF Web ACL. SiteStack consumes both cross-region via
 * `crossRegionReferences: true`.
 */
export class EdgeStack extends cdk.Stack {
  readonly certificate: acm.Certificate;
  readonly webAcl: wafv2.CfnWebACL;

  constructor(scope: Construct, id: string, props: EdgeStackProps) {
    super(scope, id, props);

    const zone = route53.HostedZone.fromHostedZoneAttributes(this, "Zone", {
      hostedZoneId: props.hostedZoneId,
      zoneName: props.siteDomain,
    });

    this.certificate = new acm.Certificate(this, "Certificate", {
      domainName: props.siteDomain,
      validation: acm.CertificateValidation.fromDns(zone),
    });

    // Mirrors the WebACL the CloudFront console auto-creates when subscribing
    // to a flat-rate plan ("CreatedByCloudFront-xxx"): three managed rule
    // groups switched from count mode (console default) to their own rule
    // actions (none override).
    this.webAcl = new wafv2.CfnWebACL(this, "WebAcl", {
      scope: "CLOUDFRONT",
      defaultAction: { allow: {} },
      visibilityConfig: {
        sampledRequestsEnabled: true,
        cloudWatchMetricsEnabled: true,
        metricName: "LambdaBenchWebAcl",
      },
      rules: [
        {
          name: "AWS-AWSManagedRulesAmazonIpReputationList",
          priority: 0,
          statement: {
            managedRuleGroupStatement: {
              vendorName: "AWS",
              name: "AWSManagedRulesAmazonIpReputationList",
            },
          },
          overrideAction: { none: {} },
          visibilityConfig: {
            sampledRequestsEnabled: true,
            cloudWatchMetricsEnabled: true,
            metricName: "AWS-AWSManagedRulesAmazonIpReputationList",
          },
        },
        {
          name: "AWS-AWSManagedRulesCommonRuleSet",
          priority: 1,
          statement: {
            managedRuleGroupStatement: {
              vendorName: "AWS",
              name: "AWSManagedRulesCommonRuleSet",
            },
          },
          overrideAction: { none: {} },
          visibilityConfig: {
            sampledRequestsEnabled: true,
            cloudWatchMetricsEnabled: true,
            metricName: "AWS-AWSManagedRulesCommonRuleSet",
          },
        },
        {
          name: "AWS-AWSManagedRulesKnownBadInputsRuleSet",
          priority: 2,
          statement: {
            managedRuleGroupStatement: {
              vendorName: "AWS",
              name: "AWSManagedRulesKnownBadInputsRuleSet",
            },
          },
          overrideAction: { none: {} },
          visibilityConfig: {
            sampledRequestsEnabled: true,
            cloudWatchMetricsEnabled: true,
            metricName: "AWS-AWSManagedRulesKnownBadInputsRuleSet",
          },
        },
      ],
    });
  }
}
