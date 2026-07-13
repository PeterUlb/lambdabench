// Shared resource-naming constants for the LambdaBench runner's IAM policy.
//
// These mirror values the bencher driver hardcodes in bencher/src/config.rs.
// They are duplicated here (not imported) because the CDK app and the Rust
// driver are separate build units, so the two MUST be kept in sync by hand:
// changing a name or tag in config.rs requires the matching edit here, or the
// least-privilege policy stops matching the resources the driver creates.

/**
 * Static prefix every LambdaBench resource name carries (bencher's
 * `config::PREFIX`). Fixed, not configurable: it scopes the runner's IAM policy
 * to `lambdabench-*` ARNs (see RESOURCE_WILDCARD), and every resource name the
 * driver derives from config starts with it, so it must stay stable across
 * build/deploy/run/teardown. (Teardown itself deletes each resource by its exact
 * config-derived name, not by listing this prefix.)
 */
export const RESOURCE_PREFIX = "lambdabench";

/**
 * Wildcard matching any LambdaBench resource name, used to scope IAM resource
 * ARNs (e.g. `function:lambdabench-*`, `table/lambdabench-*`). Broader than
 * RESOURCE_PREFIX because the driver appends per-resource segments (function
 * names, table name) after it.
 */
export const RESOURCE_WILDCARD = "lambdabench-*";

/**
 * Tag the driver applies to the KMS key at creation (bencher's `KMS_TAG_KEY` /
 * `KMS_TAG_VALUE`). The runner's `kms:ScheduleKeyDeletion` grant is scoped by
 * this tag rather than by alias: teardown removes the alias before scheduling,
 * and the orphan-cleanup path schedules a key that never received the alias, so
 * a `kms:ResourceAliases` condition would reject both. The tag is present from
 * key creation, so it matches in every path.
 *
 * Unlike every other resource here, the orphan sweep this scopes finds
 * candidates by scanning every KMS key in the account/region, not by an exact
 * name, so this tag is the only thing distinguishing a lambdabench-owned key
 * from someone else's in a shared account. Deliberately a specific, namespaced
 * phrase rather than the bare `RESOURCE_PREFIX` word, which a coincidental
 * unrelated tag could realistically match.
 */
export const KMS_TAG_KEY = "lambdabench-managed-kms-key";
export const KMS_TAG_VALUE = "true";
