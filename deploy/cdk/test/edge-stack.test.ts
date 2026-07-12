import * as cdk from "aws-cdk-lib";
import { Template, Match } from "aws-cdk-lib/assertions";
import { EdgeStack } from "../lib/edge-stack";

// Locks the security posture of the edge resources: the WAF Web ACL must
// actually enforce its managed rule groups (overrideAction `none`, not `count`),
// and the certificate must validate via DNS against the supplied zone. A
// regression to count-mode would silently turn the WAF into a no-op, so it is
// asserted explicitly.
describe("EdgeStack", () => {
  const app = new cdk.App();
  const stack = new EdgeStack(app, "TestEdgeStack", {
    env: { account: "111111111111", region: "us-east-1" },
    siteDomain: "bench.example.com",
    hostedZoneId: "Z0123456789ABCDEFGHIJ",
  });
  const template = Template.fromStack(stack);

  test("Web ACL is CLOUDFRONT-scoped and defaults to allow", () => {
    template.hasResourceProperties("AWS::WAFv2::WebACL", {
      Scope: "CLOUDFRONT",
      DefaultAction: { Allow: {} },
    });
  });

  test("all three managed rule groups enforce (overrideAction none, not count)", () => {
    const expected = [
      "AWSManagedRulesAmazonIpReputationList",
      "AWSManagedRulesCommonRuleSet",
      "AWSManagedRulesKnownBadInputsRuleSet",
    ];
    template.hasResourceProperties("AWS::WAFv2::WebACL", {
      Rules: Match.arrayWith(
        expected.map((name) =>
          Match.objectLike({
            // `none` keeps each managed group on its own rule actions; `count`
            // would observe-only and defeat the WAF. Assert it is absent.
            OverrideAction: { None: {} },
            Statement: {
              ManagedRuleGroupStatement: { VendorName: "AWS", Name: name },
            },
          }),
        ),
      ),
    });
  });

  test("certificate validates via DNS", () => {
    template.hasResourceProperties("AWS::CertificateManager::Certificate", {
      DomainName: "bench.example.com",
      ValidationMethod: "DNS",
    });
  });
});
