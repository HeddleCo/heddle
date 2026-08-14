// SPDX-License-Identifier: Apache-2.0

use heddle_semantic_recovery::{StateDocument, StateKey};

pub const DIVERGENCE_COUNT: usize = Divergence::ALL.len();
pub const SCENARIO_COUNT: usize = SCENARIOS.len();

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Divergence {
    Baseline,
    Rename,
    Reorder,
    Insert,
    CommentChurn,
    RenameReorder,
    Mixed,
}

impl Divergence {
    pub const ALL: [Self; 7] = [
        Self::Baseline,
        Self::Rename,
        Self::Reorder,
        Self::Insert,
        Self::CommentChurn,
        Self::RenameReorder,
        Self::Mixed,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Rename => "rename",
            Self::Reorder => "reorder",
            Self::Insert => "insert",
            Self::CommentChurn => "comment-churn",
            Self::RenameReorder => "rename+reorder",
            Self::Mixed => "mixed",
        }
    }
}

#[derive(Clone, Copy)]
struct Scenario {
    slug: &'static str,
    entity: &'static str,
    operation: &'static str,
    field: &'static str,
    constraint: &'static str,
    failure: &'static str,
}

const SCENARIOS: [Scenario; 18] = [
    Scenario::new(
        "jwt-refresh",
        "RefreshToken",
        "rotate_token",
        "expiry",
        "single use",
        "token replay",
    ),
    Scenario::new(
        "invoice-proration",
        "Invoice",
        "prorate_charge",
        "billing_period",
        "currency precision",
        "negative balance",
    ),
    Scenario::new(
        "dns-cache",
        "DnsRecord",
        "refresh_lookup",
        "ttl",
        "stale while revalidate",
        "resolver timeout",
    ),
    Scenario::new(
        "image-resize",
        "ImageAsset",
        "resize_thumbnail",
        "dimensions",
        "preserve aspect ratio",
        "decode failure",
    ),
    Scenario::new(
        "lease-renewal",
        "WriterLease",
        "renew_lease",
        "deadline",
        "exclusive writer",
        "expired lease",
    ),
    Scenario::new(
        "rate-limit",
        "TokenBucket",
        "admit_request",
        "capacity",
        "monotonic refill",
        "quota exceeded",
    ),
    Scenario::new(
        "schema-migrate",
        "SchemaVersion",
        "apply_migration",
        "generation",
        "forward only",
        "version mismatch",
    ),
    Scenario::new(
        "log-redaction",
        "AuditEvent",
        "redact_secret",
        "payload",
        "preserve evidence",
        "invalid range",
    ),
    Scenario::new(
        "route-match",
        "RoutePattern",
        "match_request",
        "segments",
        "percent decoding",
        "ambiguous route",
    ),
    Scenario::new(
        "backup-retention",
        "BackupSet",
        "prune_snapshot",
        "retention",
        "keep monthly",
        "protected snapshot",
    ),
    Scenario::new(
        "search-ranking",
        "SearchCandidate",
        "rank_results",
        "relevance",
        "stable tie break",
        "invalid score",
    ),
    Scenario::new(
        "email-dedup",
        "MailEnvelope",
        "deduplicate_message",
        "message_id",
        "tenant isolation",
        "duplicate delivery",
    ),
    Scenario::new(
        "geo-fence",
        "GeoFence",
        "contains_coordinate",
        "polygon",
        "antimeridian safe",
        "invalid latitude",
    ),
    Scenario::new(
        "csv-import",
        "CsvRow",
        "validate_record",
        "columns",
        "header mapping",
        "malformed record",
    ),
    Scenario::new(
        "feature-rollout",
        "FeatureGate",
        "select_variant",
        "allocation",
        "sticky assignment",
        "unknown cohort",
    ),
    Scenario::new(
        "job-retry",
        "RetryPolicy",
        "schedule_retry",
        "backoff",
        "bounded jitter",
        "retry exhausted",
    ),
    Scenario::new(
        "cache-eviction",
        "CacheEntry",
        "evict_candidate",
        "last_access",
        "pinned entries",
        "capacity pressure",
    ),
    Scenario::new(
        "webhook-signing",
        "Webhook",
        "verify_signature",
        "signature",
        "constant time",
        "digest mismatch",
    ),
];

impl Scenario {
    const fn new(
        slug: &'static str,
        entity: &'static str,
        operation: &'static str,
        field: &'static str,
        constraint: &'static str,
        failure: &'static str,
    ) -> Self {
        Self {
            slug,
            entity,
            operation,
            field,
            constraint,
            failure,
        }
    }
}

pub fn fixture() -> (Vec<StateDocument>, Vec<Divergence>) {
    let mut documents = Vec::new();
    let mut classes = Vec::new();
    for scenario in SCENARIOS {
        for class in Divergence::ALL {
            let seed = format!("{}:{}", scenario.slug, class.name());
            documents.push(StateDocument {
                state: StateKey(*blake3::hash(seed.as_bytes()).as_bytes()),
                thread: scenario.slug.to_string(),
                text: render(scenario, class),
            });
            classes.push(class);
        }
    }
    (documents, classes)
}

fn render(scenario: Scenario, class: Divergence) -> String {
    let renamed = matches!(
        class,
        Divergence::Rename | Divergence::RenameReorder | Divergence::Mixed
    );
    let reordered = matches!(
        class,
        Divergence::Reorder | Divergence::RenameReorder | Divergence::Mixed
    );
    let inserted = matches!(class, Divergence::Insert | Divergence::Mixed);
    let churned = matches!(class, Divergence::CommentChurn | Divergence::Mixed);
    let entity = if renamed {
        "Coordinator"
    } else {
        scenario.entity
    };
    let operation = if renamed {
        "execute_policy"
    } else {
        scenario.operation
    };
    let path = if renamed {
        "src/policy/handler.rs".to_string()
    } else {
        format!("src/{}.rs", scenario.entity.to_ascii_lowercase())
    };
    let comments = if churned {
        "// Keep this operation deterministic and return a typed failure."
    } else {
        "// Enforce the domain invariant before committing the transition."
    };
    let validate = format!(
        "fn validate_{field}(value: &str) -> Result<(), Error> {{\n    if value.is_empty() {{ return Err(Error::new(\"{failure}\")); }}\n    Ok(())\n}}",
        field = scenario.field,
        failure = scenario.failure,
    );
    let apply = format!(
        "pub fn {operation}(item: &mut {entity}, value: &str) -> Result<(), Error> {{\n    validate_{field}(value)?;\n    item.{field} = value.to_owned();\n    item.audit(\"{constraint}\");\n    Ok(())\n}}",
        operation = operation,
        entity = entity,
        field = scenario.field,
        constraint = scenario.constraint,
    );
    let functions = if reordered {
        format!("{apply}\n\n{validate}")
    } else {
        format!("{validate}\n\n{apply}")
    };
    let insertion = if inserted {
        "\nfn emit_metric(name: &str) { metrics::increment(name); }\nfn trace_latency() -> Guard { Guard::start() }\n"
    } else {
        ""
    };
    format!(
        "intent: {operation} while enforcing {constraint}\npath: {path}\n{comments}\npub struct {entity} {{ pub {field}: String }}\n{insertion}{functions}\n",
        constraint = scenario.constraint,
        field = scenario.field,
    )
}
