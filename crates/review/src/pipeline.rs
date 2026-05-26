// SPDX-License-Identifier: Apache-2.0
use uuid::Uuid;

use crate::{
    Result,
    errors::ReviewError,
    github::{
        auth::{GitHubAuthClient, GitHubInstallationLookup},
        rest::{GitHubPullRequestFile, GitHubRestClient},
    },
    store::traits::{ReviewJobStore, ReviewProgressUpdate},
    types::{
        ReviewAnalysisRequest, ReviewAnalysisResult, ReviewFileArtifact, ReviewNoiseFile,
        ReviewPacket, ReviewSemanticChange,
    },
};

const MAX_PATCH_LINES: usize = 500;

#[derive(Clone)]
struct ChangeCandidate {
    change_type: String,
    description: String,
    path: String,
    additions: u32,
    deletions: u32,
    patch: Option<String>,
    impact: u32,
}

pub struct ReviewPipeline<S, L> {
    store: S,
    lookup: L,
    github_auth: GitHubAuthClient,
    github_rest: GitHubRestClient,
}

impl<S, L> ReviewPipeline<S, L>
where
    S: ReviewJobStore,
    L: GitHubInstallationLookup,
{
    pub fn new(
        store: S,
        lookup: L,
        github_auth: GitHubAuthClient,
        github_rest: GitHubRestClient,
    ) -> Self {
        Self {
            store,
            lookup,
            github_auth,
            github_rest,
        }
    }

    pub async fn run(&self, job_id: Uuid, request: &ReviewAnalysisRequest) -> Result<()> {
        let key = request.key();
        let installation_id = self.lookup.installation_id_for_repo(&key).await?;
        let token = if let Some(installation_id) = installation_id {
            Some(self.github_auth.installation_token(installation_id).await?)
        } else {
            None
        };

        self.store
            .update_progress(
                job_id,
                ReviewProgressUpdate::fetching("Fetching PR contents…"),
            )
            .await?;

        let fetched = self
            .github_rest
            .fetch_pull_request(&key, token.as_deref())
            .await
            .map_err(|err| match (installation_id, err) {
                (None, ReviewError::Github(message)) if message.contains("404") => {
                    ReviewError::Github(
                        "public fallback could not read this pull request; it may be private or missing"
                            .to_string(),
                    )
                }
                (_, other) => other,
            })?;

        self.store
            .attach_metadata(job_id, &fetched.metadata.head_sha, &fetched.metadata)
            .await?;

        let packet = classify_files(&fetched.files, &fetched.contributors);
        let files = build_artifacts(&packet, &fetched.files);

        self.store
            .update_progress(
                job_id,
                ReviewProgressUpdate::finalizing(
                    files.len() as u32,
                    fetched.metadata.head_sha.clone(),
                    "Finalizing review…",
                ),
            )
            .await?;

        let result = ReviewAnalysisResult {
            job_id,
            key,
            head_sha: fetched.metadata.head_sha.clone(),
            metadata: fetched.metadata,
            packet,
            comments: fetched.comments,
            files,
            completed_at: Some(chrono::Utc::now()),
        };

        self.store.complete_job(job_id, &result).await
    }

    pub async fn fail(&self, job_id: Uuid, head_sha: Option<String>, error: &str) -> Result<()> {
        self.store
            .update_progress(
                job_id,
                ReviewProgressUpdate::failed(error, head_sha, "Analysis failed"),
            )
            .await
    }
}

fn classify_files(
    files: &[GitHubPullRequestFile],
    contributors: &[crate::types::ReviewContributor],
) -> ReviewPacket {
    let mut changes = Vec::<ChangeCandidate>::new();
    let mut noise = Vec::new();

    for file in files {
        let ext = file
            .filename
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let name = file.filename.rsplit('/').next().unwrap_or_default();
        let dir = file
            .filename
            .rsplit_once('/')
            .map(|(prefix, _)| prefix)
            .unwrap_or_default();
        let total_lines = file.additions + file.deletions;

        if is_formatting_only(file.patch.as_deref()) {
            noise.push(ReviewNoiseFile {
                path: file.filename.clone(),
                reason: "formatting".to_string(),
                additions: file.additions,
                deletions: file.deletions,
            });
            continue;
        }
        if matches!(name, "bun.lock" | "yarn.lock" | "pnpm-lock.yaml") || ext == "lock" {
            noise.push(ReviewNoiseFile {
                path: file.filename.clone(),
                reason: "lockfile".to_string(),
                additions: file.additions,
                deletions: file.deletions,
            });
            continue;
        }
        if file.status == "renamed" && total_lines == 0 {
            noise.push(ReviewNoiseFile {
                path: file.filename.clone(),
                reason: "rename-only".to_string(),
                additions: file.additions,
                deletions: file.deletions,
            });
            continue;
        }
        if total_lines <= 2 && file.status == "modified" {
            let signals = file.patch.as_deref().map(analyze_patch);
            let has_semantic = signals.as_ref().is_some_and(|signals| {
                !signals.added_defs.is_empty()
                    || !signals.removed_defs.is_empty()
                    || !signals.schema_ops.is_empty()
            });
            if !has_semantic && !is_risk_path(&file.filename) {
                noise.push(ReviewNoiseFile {
                    path: file.filename.clone(),
                    reason: "trivial".to_string(),
                    additions: file.additions,
                    deletions: file.deletions,
                });
                continue;
            }
        }
        if dir.contains("/gen/")
            || name.ends_with("_pb.ts")
            || name.ends_with(".pb.go")
            || name.contains(".generated.")
        {
            noise.push(ReviewNoiseFile {
                path: file.filename.clone(),
                reason: "generated".to_string(),
                additions: file.additions,
                deletions: file.deletions,
            });
            continue;
        }

        let change_type = classify_file_type(&file.filename, &ext, name, dir);
        changes.push(ChangeCandidate {
            change_type,
            description: describe_change(file),
            path: file.filename.clone(),
            additions: file.additions,
            deletions: file.deletions,
            patch: file.patch.clone(),
            impact: 0,
        });
    }

    let semantic_change_count = changes.len() as u32;
    let mut merged = Vec::<ChangeCandidate>::new();
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    for change in changes {
        let key = format!("{}:{}", change.change_type, change.description);
        if let Some(index) = seen.get(&key).copied() {
            merged[index].additions += change.additions;
            merged[index].deletions += change.deletions;
            match (&mut merged[index].patch, change.patch) {
                (Some(existing), Some(patch)) => {
                    existing.push('\n');
                    existing.push_str(&patch);
                }
                (None, Some(patch)) => merged[index].patch = Some(patch),
                _ => {}
            }
            if change.path.len() < merged[index].path.len() {
                merged[index].path = change.path;
            }
        } else {
            seen.insert(key, merged.len());
            merged.push(change);
        }
    }

    let raw_scores = merged.iter().map(score_change_impact).collect::<Vec<_>>();
    let max_score = raw_scores.iter().copied().fold(1.0_f64, f64::max);
    for (change, raw_score) in merged.iter_mut().zip(raw_scores) {
        change.impact = ((raw_score / max_score) * 100.0).round() as u32;
    }
    merged.sort_by(|left, right| {
        right.impact.cmp(&left.impact).then_with(|| {
            let order = |kind: &str| match kind {
                "schema" => 0,
                "api" => 1,
                "logic" => 2,
                "infra" => 3,
                "config" => 4,
                "docs" => 5,
                "test" => 6,
                _ => 7,
            };
            order(&left.change_type).cmp(&order(&right.change_type))
        })
    });

    let agents = contributors
        .iter()
        .filter(|contributor| contributor.is_agent)
        .count() as u32;
    let humans = contributors.len() as u32 - agents;

    ReviewPacket {
        files_changed: files.len() as u32,
        semantic_changes: semantic_change_count,
        noise_filtered: noise.len() as u32,
        narrative: build_narrative(&merged),
        changes: merged
            .into_iter()
            .map(|change| ReviewSemanticChange {
                change_type: change.change_type,
                description: change.description,
                path: change.path,
                additions: change.additions,
                deletions: change.deletions,
                impact: change.impact,
            })
            .collect(),
        noise,
        contributors: contributors.to_vec(),
        agents,
        humans,
    }
}

fn build_artifacts(
    packet: &ReviewPacket,
    files: &[GitHubPullRequestFile],
) -> Vec<ReviewFileArtifact> {
    let patch_by_path = files
        .iter()
        .map(|file| (file.filename.as_str(), file.patch.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    packet
        .changes
        .iter()
        .enumerate()
        .map(|(index, change)| {
            let original_patch = patch_by_path.get(change.path.as_str()).cloned().flatten();
            let (patch, truncated) = truncate_patch(original_patch);
            ReviewFileArtifact {
                index: index as u32,
                change_type: change.change_type.clone(),
                description: change.description.clone(),
                path: change.path.clone(),
                additions: change.additions,
                deletions: change.deletions,
                impact: change.impact,
                patch,
                truncated,
                highlights: None,
            }
        })
        .collect()
}

fn truncate_patch(patch: Option<String>) -> (Option<String>, bool) {
    let Some(patch) = patch else {
        return (None, false);
    };
    let lines = patch.lines().collect::<Vec<_>>();
    if lines.len() <= MAX_PATCH_LINES {
        return (Some(patch), false);
    }
    (Some(lines[..MAX_PATCH_LINES].join("\n")), true)
}

fn is_formatting_only(patch: Option<&str>) -> bool {
    let Some(patch) = patch else {
        return false;
    };
    let mut hunks = Vec::<(Vec<String>, Vec<String>)>::new();
    let mut current: Option<(Vec<String>, Vec<String>)> = None;
    for line in patch.lines() {
        if line.starts_with("@@") {
            if let Some(current_hunk) = current.take() {
                hunks.push(current_hunk);
            }
            current = Some((Vec::new(), Vec::new()));
            continue;
        }
        let Some((removed, added)) = current.as_mut() else {
            continue;
        };
        if line.starts_with('-') && !line.starts_with("---") {
            removed.push(line[1..].to_string());
        } else if line.starts_with('+') && !line.starts_with("+++") {
            added.push(line[1..].to_string());
        }
    }
    if let Some(current_hunk) = current.take() {
        hunks.push(current_hunk);
    }
    !hunks.is_empty()
        && hunks
            .iter()
            .all(|(removed, added)| is_chunk_formatting(removed, added))
}

fn is_risk_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    [
        "auth",
        "secret",
        "token",
        "password",
        "permission",
        "session",
        "payment",
        "billing",
        "security",
    ]
    .iter()
    .any(|needle| path.contains(needle))
}

fn classify_file_type(_path: &str, ext: &str, name: &str, dir: &str) -> String {
    if ext == "sql" || dir.contains("migration") || name.contains("migration") {
        return "schema".to_string();
    }
    if dir.contains("/api/") || dir.contains("/routes/") || dir.contains("/endpoints/") {
        return "api".to_string();
    }
    if name.contains("+server.") || name.contains("+page.server.") {
        return "api".to_string();
    }
    if name.contains(".test.")
        || name.contains(".spec.")
        || dir.contains("__tests__")
        || dir.contains("/test/")
    {
        return "test".to_string();
    }
    if matches!(
        name,
        "Dockerfile" | "docker-compose.yml" | "docker-compose.yaml" | ".dockerignore"
    ) || dir.contains("/.github/")
        || dir.contains("/.circleci/")
        || dir.ends_with("scripts")
        || name.ends_with(".sh")
        || matches!(name, "Makefile" | "Justfile")
    {
        return "infra".to_string();
    }
    if matches!(ext, "toml" | "yaml" | "yml")
        || matches!(
            name,
            ".gitignore"
                | ".env.example"
                | ".editorconfig"
                | "package.json"
                | "Cargo.toml"
                | "tsconfig.json"
        )
        || name.ends_with(".config.js")
        || name.ends_with(".config.ts")
        || is_config_name(name)
    {
        return "config".to_string();
    }
    if matches!(ext, "md" | "mdx" | "rst" | "txt") {
        return "docs".to_string();
    }
    if matches!(
        name,
        "LICENSE" | "CHANGELOG" | "CHANGES" | "AUTHORS" | "CONTRIBUTORS"
    ) || dir.contains("/docs/")
        || dir.contains("/documentation/")
    {
        return "docs".to_string();
    }
    if name.contains("+page.svelte") || name.contains("+layout.svelte") {
        return "api".to_string();
    }
    "logic".to_string()
}

fn describe_change(file: &GitHubPullRequestFile) -> String {
    let ext = file
        .filename
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = file.filename.rsplit('/').next().unwrap_or_default();
    let dir = file
        .filename
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or_default();
    let change_type = classify_file_type(&file.filename, &ext, name, dir);
    let verb = match file.status.as_str() {
        "added" => "Add",
        "removed" => "Remove",
        _ => "Update",
    };
    if change_type == "schema" {
        return format!("{} database migration `{}`", verb, name);
    }
    if change_type == "api" {
        let route = dir.strip_prefix("web/src/routes").unwrap_or(dir);
        if name.contains("+server.") {
            return format!(
                "{} API route `{}`",
                verb,
                if route.is_empty() { "/" } else { route }
            );
        }
        if name == "+page.server.ts" {
            return format!(
                "{} server loader `{}`",
                verb,
                if route.is_empty() { "/" } else { route }
            );
        }
        if name == "+page.svelte" {
            return format!(
                "{} page UI `{}`",
                verb,
                if route.is_empty() { "/" } else { route }
            );
        }
        if name == "+layout.svelte" {
            return format!(
                "{} layout `{}`",
                verb,
                if route.is_empty() { "/" } else { route }
            );
        }
        return format!(
            "{} route `{}`",
            verb,
            if route.is_empty() { "/" } else { route }
        );
    }
    if change_type == "test" {
        return format!("{} tests in `{}`", verb, name);
    }
    if change_type == "config" {
        return format!("{} `{}`", verb, name);
    }
    let module = module_from_path(&file.filename);
    if let Some(patch) = file.patch.as_deref() {
        return extract_semantic_description(&file.filename, &file.status, patch, &module);
    }
    if module == name {
        format!("{} `{}`", verb, name)
    } else {
        format!("{} `{}` in {}", verb, name, module)
    }
}

fn score_change_impact(change: &ChangeCandidate) -> f64 {
    let mut score = 0.0_f64;
    let total_lines = (change.additions + change.deletions) as f64;
    score += (total_lines + 1.0).log2() * 5.0;
    score = score.min(40.0);
    score += match change.change_type.as_str() {
        "schema" => 30.0,
        "api" => 25.0,
        "logic" => 20.0,
        "test" => 10.0,
        "infra" => 8.0,
        "config" => 5.0,
        "docs" => 3.0,
        _ => 15.0,
    };
    if change.description.starts_with("Add ") {
        score += 15.0;
    } else if change.description.starts_with("Remove ") {
        score += 10.0;
    }
    if is_risk_path(&change.path) || is_risk_path(&change.description) {
        score += 15.0;
    }
    if let Some(patch) = change.patch.as_deref() {
        let code_lines = patch
            .lines()
            .filter(|line| {
                (line.starts_with('+') || line.starts_with('-'))
                    && !line.starts_with("+++")
                    && !line.starts_with("---")
            })
            .collect::<Vec<_>>();
        let non_blank = code_lines.iter().filter(|line| !line.is_empty()).count() as f64;
        let density = if code_lines.is_empty() {
            0.0
        } else {
            non_blank / code_lines.len() as f64
        };
        score += density * 10.0;
    }
    if is_penalty_path(&change.path) {
        score *= 0.1;
    }
    if total_lines > 500.0 && matches!(change.change_type.as_str(), "config" | "infra") {
        score *= 0.3;
    }
    score
}

fn build_narrative(changes: &[ChangeCandidate]) -> String {
    let Some(primary) = changes.first() else {
        return String::new();
    };
    if changes.len() == 1 {
        return format!("Focuses on {}.", primary.description.to_ascii_lowercase());
    }
    format!(
        "Focuses on {}, plus {} more review areas.",
        primary.description.to_ascii_lowercase(),
        changes.len() - 1
    )
}

fn is_penalty_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".lock")
        || path.contains(".generated.")
        || path.ends_with("_pb.ts")
        || path.ends_with(".pb.go")
        || path.contains("/gen/")
        || path.ends_with(".min.js")
        || path.ends_with(".min.css")
        || path.contains("vendor/")
        || path.ends_with("package-lock.json")
        || path.ends_with("bun.lock")
        || path.ends_with("yarn.lock")
}

fn is_config_name(name: &str) -> bool {
    let mut parts = name.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some("config"), Some(_), None)
    )
}

fn module_from_path(filename: &str) -> String {
    let mut parts = filename
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let name = parts.pop().unwrap_or(filename);
    let dir = parts.into_iter().rev().take(2).collect::<Vec<_>>();
    if dir.is_empty() {
        name.to_string()
    } else {
        dir.into_iter().rev().collect::<Vec<_>>().join("/")
    }
}

#[derive(Default)]
struct PatchSignals {
    added_defs: std::collections::BTreeMap<String, String>,
    removed_defs: std::collections::BTreeMap<String, String>,
    hunk_scopes: Vec<String>,
    schema_ops: Vec<(String, String)>,
}

fn analyze_patch(patch: &str) -> PatchSignals {
    let mut signals = PatchSignals::default();
    for line in patch.lines() {
        if let Some(scope_text) = line
            .strip_prefix("@@")
            .and_then(|rest| rest.rsplit("@@").next())
            .map(str::trim)
        {
            for re in [
                regex_lite::Regex::new(r"(?:pub\s+)?fn\s+(\w+)").unwrap(),
                regex_lite::Regex::new(r"(?:pub\s+)?struct\s+(\w+)").unwrap(),
                regex_lite::Regex::new(r"impl(?:<[^>]+>)?\s+(\w+)").unwrap(),
                regex_lite::Regex::new(r"(?:export\s+)?(?:async\s+)?function\s+(\w+)").unwrap(),
                regex_lite::Regex::new(r"(?:export\s+)?class\s+(\w+)").unwrap(),
                regex_lite::Regex::new(r"(?:async\s+)?def\s+(\w+)").unwrap(),
                regex_lite::Regex::new(r"func\s+(?:\([^)]+\)\s+)?(\w+)").unwrap(),
                regex_lite::Regex::new(r"type\s+(\w+)\s+(?:struct|interface)").unwrap(),
            ] {
                if let Some(captures) = re.captures(scope_text) {
                    let name = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
                    if !name.is_empty() && !signals.hunk_scopes.iter().any(|scope| scope == name) {
                        signals.hunk_scopes.push(name.to_string());
                    }
                    break;
                }
            }
            continue;
        }
        if (!line.starts_with('+') && !line.starts_with('-'))
            || line.starts_with("+++")
            || line.starts_with("---")
        {
            continue;
        }
        let is_add = line.starts_with('+');
        let content = &line[1..];
        for (re, kind) in definition_patterns() {
            if let Some(captures) = re.captures(content) {
                let name = captures
                    .get(if kind == "route" && captures.len() > 2 {
                        2
                    } else {
                        1
                    })
                    .map(|m| m.as_str())
                    .unwrap_or_default();
                if !name.is_empty() {
                    let target = if is_add {
                        &mut signals.added_defs
                    } else {
                        &mut signals.removed_defs
                    };
                    target
                        .entry(name.to_string())
                        .or_insert_with(|| kind.to_string());
                }
                break;
            }
        }
        if is_add {
            if let Some(captures) = regex_lite::Regex::new(r"ADD\s+(?:COLUMN\s+)?(\w+)")
                .unwrap()
                .captures(content)
            {
                signals
                    .schema_ops
                    .push(("add_column".to_string(), captures[1].to_string()));
            }
            if let Some(captures) =
                regex_lite::Regex::new(r"CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?(\w+)")
                    .unwrap()
                    .captures(content)
            {
                signals
                    .schema_ops
                    .push(("create_table".to_string(), captures[1].to_string()));
            }
            if let Some(captures) = regex_lite::Regex::new(r"ALTER\s+TABLE\s+(\w+)")
                .unwrap()
                .captures(content)
            {
                signals
                    .schema_ops
                    .push(("alter_table".to_string(), captures[1].to_string()));
            }
        }
    }
    signals
}

fn definition_patterns() -> Vec<(regex_lite::Regex, &'static str)> {
    vec![
        (
            regex_lite::Regex::new(r"pub(?:\(crate\))?\s+fn\s+(\w+)").unwrap(),
            "fn",
        ),
        (
            regex_lite::Regex::new(r"pub\s+struct\s+(\w+)").unwrap(),
            "type",
        ),
        (
            regex_lite::Regex::new(r"pub\s+enum\s+(\w+)").unwrap(),
            "type",
        ),
        (
            regex_lite::Regex::new(r"pub\s+trait\s+(\w+)").unwrap(),
            "type",
        ),
        (
            regex_lite::Regex::new(r"impl(?:<[^>]+>)?\s+(\w+)").unwrap(),
            "type",
        ),
        (regex_lite::Regex::new(r"pub\s+mod\s+(\w+)").unwrap(), "fn"),
        (
            regex_lite::Regex::new(r"export\s+(?:async\s+)?function\s+(\w+)").unwrap(),
            "fn",
        ),
        (
            regex_lite::Regex::new(r"export\s+class\s+(\w+)").unwrap(),
            "type",
        ),
        (
            regex_lite::Regex::new(r"export\s+(?:const|let)\s+(\w+)\s*=").unwrap(),
            "fn",
        ),
        (
            regex_lite::Regex::new(r"export\s+type\s+(\w+)").unwrap(),
            "type",
        ),
        (
            regex_lite::Regex::new(r"export\s+interface\s+(\w+)").unwrap(),
            "type",
        ),
        (
            regex_lite::Regex::new(r"(?:async\s+)?def\s+(\w+)").unwrap(),
            "fn",
        ),
        (regex_lite::Regex::new(r"class\s+(\w+)").unwrap(), "type"),
        (
            regex_lite::Regex::new(r"func\s+(?:\([^)]+\)\s+)?(\w+)\s*\(").unwrap(),
            "fn",
        ),
        (
            regex_lite::Regex::new(r"type\s+(\w+)\s+struct").unwrap(),
            "type",
        ),
        (
            regex_lite::Regex::new(r"type\s+(\w+)\s+interface").unwrap(),
            "type",
        ),
        (
            regex_lite::Regex::new(r"CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?(\w+)").unwrap(),
            "schema",
        ),
        (
            regex_lite::Regex::new(r"ALTER\s+TABLE\s+(\w+)").unwrap(),
            "schema",
        ),
        (
            regex_lite::Regex::new(r"ADD\s+(?:COLUMN\s+)?(\w+)").unwrap(),
            "schema",
        ),
        (
            regex_lite::Regex::new(r#"app\.(get|post|put|delete|patch)\s*\(\s*['"`]([^'"`]+)"#)
                .unwrap(),
            "route",
        ),
        (
            regex_lite::Regex::new(r#"router\.(get|post|put|delete|patch)\s*\(\s*['"`]([^'"`]+)"#)
                .unwrap(),
            "route",
        ),
    ]
}

fn extract_semantic_description(filename: &str, status: &str, patch: &str, module: &str) -> String {
    let name = filename.rsplit('/').next().unwrap_or(filename);
    let verb = match status {
        "added" => "Add",
        "removed" => "Remove",
        _ => "Update",
    };
    let signals = analyze_patch(patch);
    if status == "added" {
        let types = signals
            .added_defs
            .iter()
            .filter(|(_, kind)| kind.as_str() == "type")
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        if !types.is_empty() {
            return format!("Add {}", list_names(&types, false));
        }
        let fns = signals
            .added_defs
            .iter()
            .filter(|(_, kind)| kind.as_str() == "fn")
            .map(|(name, _)| format!("{name}()"))
            .collect::<Vec<_>>();
        if !fns.is_empty() {
            return format!("Add {}", list_names(&fns, false));
        }
        let routes = signals
            .added_defs
            .iter()
            .filter(|(_, kind)| kind.as_str() == "route")
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        if !routes.is_empty() {
            return format!("Add route {}", list_names(&routes, false));
        }
        let schemas = signals
            .added_defs
            .iter()
            .filter(|(_, kind)| kind.as_str() == "schema")
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        if !schemas.is_empty() {
            return format!("Create {}", list_names(&schemas, false));
        }
        return format!("Add `{name}`");
    }
    if status == "removed" {
        return format!("Remove `{name}`");
    }
    let new_defs = signals
        .added_defs
        .iter()
        .filter(|(name, _)| !signals.removed_defs.contains_key(name.as_str()))
        .map(|(name, kind)| {
            if kind == "fn" {
                format!("{name}()")
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>();
    if !new_defs.is_empty() {
        return format!("Add {} to {}", list_names(&new_defs, false), module);
    }
    if let Some((op, primary_name)) = signals.schema_ops.first() {
        let names = signals
            .schema_ops
            .iter()
            .map(|(_, name)| name.clone())
            .collect::<Vec<_>>();
        let verb = match op.as_str() {
            "create_table" => "Create",
            "alter_table" => "Alter",
            "add_column" => "Add column",
            _ => "Update",
        };
        let _ = primary_name;
        return format!("{verb} {}", list_names(&names, false));
    }
    if let Some(primary) = signals.hunk_scopes.first() {
        let extra = if signals.hunk_scopes.len() > 1 {
            format!(" and {} more", signals.hunk_scopes.len() - 1)
        } else {
            String::new()
        };
        let refactored = signals
            .added_defs
            .keys()
            .any(|name| signals.removed_defs.contains_key(name));
        if refactored {
            return format!("Refactor `{primary}()`{extra} in {module}");
        }
        return format!("Update `{primary}()`{extra} in {module}");
    }
    if module == name {
        format!("{verb} `{name}`")
    } else {
        format!("{verb} `{name}` in {module}")
    }
}

fn list_names(names: &[String], wrap_ticks: bool) -> String {
    let wrap = |name: &str| {
        if wrap_ticks {
            format!("`{name}`")
        } else if name.contains('`') {
            name.to_string()
        } else {
            format!("`{name}`")
        }
    };
    match names {
        [] => String::new(),
        [one] => wrap(one),
        [one, two] => format!("{} and {}", wrap(one), wrap(two)),
        [one, two, rest @ ..] => format!("{}, {}, and {} more", wrap(one), wrap(two), rest.len()),
    }
}

fn normalize_line(line: &str) -> String {
    let mut result = line
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(['"', '`'], "'")
        .trim_end_matches(';')
        .trim_end_matches(',')
        .to_string();
    loop {
        let updated = regex_lite::Regex::new(r",\s*([)\]}])")
            .unwrap()
            .replace_all(&result, "$1")
            .to_string();
        if updated == result {
            break;
        }
        result = updated;
    }
    regex_lite::Regex::new(r"(^|[\s{,])'(\w+)'\s*:")
        .unwrap()
        .replace_all(&result, "$1$2:")
        .to_string()
}

fn is_chunk_formatting(removed: &[String], added: &[String]) -> bool {
    if removed.is_empty() && added.is_empty() {
        return true;
    }
    let all_lines = removed.iter().chain(added.iter()).collect::<Vec<_>>();
    if all_lines.iter().all(|line| {
        let trimmed = line.trim();
        trimmed.is_empty()
            || trimmed.starts_with("import ")
            || trimmed.starts_with("from ")
            || trimmed.starts_with("use ")
            || trimmed.starts_with("require(")
    }) {
        return true;
    }
    let comment_patterns = ["//", "/*", "*", "*/", "#", "--", "<!--", "-->"];
    if all_lines.iter().all(|line| {
        let trimmed = line.trim();
        trimmed.is_empty()
            || comment_patterns
                .iter()
                .any(|pattern| trimmed.starts_with(pattern))
    }) {
        return true;
    }
    if removed.len() == added.len()
        && !removed.is_empty()
        && removed
            .iter()
            .zip(added)
            .all(|(left, right)| normalize_line(left) == normalize_line(right))
    {
        return true;
    }
    !removed.is_empty()
        && !added.is_empty()
        && normalize_line(&removed.join(" ")) == normalize_line(&added.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ReviewContributor;

    fn file(
        name: &str,
        status: &str,
        additions: u32,
        deletions: u32,
        patch: Option<&str>,
    ) -> GitHubPullRequestFile {
        GitHubPullRequestFile {
            filename: name.to_string(),
            status: status.to_string(),
            additions,
            deletions,
            patch: patch.map(str::to_string),
        }
    }

    #[test]
    fn classify_matches_ts_noise_and_type_heuristics() {
        let packet = classify_files(
            &[
                file(
                    "src/api/users/+server.ts",
                    "modified",
                    10,
                    2,
                    Some("@@ -1 +1 @@\n+export async function getUser() {}"),
                ),
                file(
                    "db/migrations/001_init.sql",
                    "added",
                    2,
                    0,
                    Some("@@ -0,0 +1,2 @@\n+CREATE TABLE users (\n+ id bigint\n+)"),
                ),
                file(
                    "docs/README.md",
                    "modified",
                    3,
                    1,
                    Some("@@ -1 +1 @@\n-Old\n+New"),
                ),
                file("bun.lock", "modified", 1, 1, Some("@@ -1 +1 @@\n-a\n+b")),
                file("src/foo.ts", "modified", 1, 1, Some("@@ -1 +1 @@\n- \n+  ")),
            ],
            &[],
        );
        assert_eq!(packet.files_changed, 5);
        assert_eq!(packet.noise_filtered, 2);
        assert_eq!(packet.changes.len(), 3);
        assert!(
            packet
                .changes
                .iter()
                .any(|change| change.change_type == "api")
        );
        assert!(
            packet
                .changes
                .iter()
                .any(|change| change.change_type == "schema")
        );
        assert!(
            packet
                .changes
                .iter()
                .any(|change| change.change_type == "docs")
        );
    }

    #[test]
    fn classify_keeps_small_semantic_or_risky_changes() {
        let packet = classify_files(
            &[
                file(
                    "db/schema.sql",
                    "modified",
                    1,
                    1,
                    Some(
                        "@@ -1 +1 @@\n-ALTER TABLE users DROP COLUMN old_name;\n+ALTER TABLE users ADD COLUMN new_name text;",
                    ),
                ),
                file(
                    "src/auth/session.ts",
                    "modified",
                    1,
                    1,
                    Some("@@ -1 +1 @@\n-const token = old;\n+const token = next;"),
                ),
            ],
            &[],
        );
        assert_eq!(packet.noise_filtered, 0);
        assert_eq!(packet.changes.len(), 2);
    }

    #[test]
    fn classify_dedupes_and_normalizes_impact() {
        let packet = classify_files(
            &[
                file(
                    "src/lib/foo.ts",
                    "modified",
                    3,
                    0,
                    Some("@@ -1 +1 @@ foo\n+export function same() {}"),
                ),
                file(
                    "src/lib/bar.ts",
                    "modified",
                    5,
                    0,
                    Some("@@ -1 +1 @@ bar\n+export function same() {}"),
                ),
            ],
            &[],
        );
        assert_eq!(packet.semantic_changes, 2);
        assert_eq!(packet.changes.len(), 1);
        assert_eq!(packet.changes[0].additions, 8);
        assert_eq!(packet.changes[0].deletions, 0);
        assert!(packet.changes[0].impact <= 100);
    }

    #[test]
    fn classify_counts_agent_and_human_contributors() {
        let packet = classify_files(
            &[file(
                "src/foo.ts",
                "modified",
                3,
                1,
                Some("@@ -1 +1 @@\n-export fn old() {}\n+export fn new() {}"),
            )],
            &[
                ReviewContributor {
                    name: "dependabot[bot]".into(),
                    email: "bot@example.com".into(),
                    is_agent: true,
                },
                ReviewContributor {
                    name: "alice".into(),
                    email: "alice@example.com".into(),
                    is_agent: false,
                },
            ],
        );
        assert_eq!(packet.agents, 1);
        assert_eq!(packet.humans, 1);
    }

    #[test]
    fn artifacts_truncate_long_patches() {
        let packet = classify_files(
            &[file(
                "src/foo.ts",
                "modified",
                600,
                0,
                Some(
                    &std::iter::repeat_n("+line", 600)
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            )],
            &[],
        );
        let files = build_artifacts(
            &packet,
            &[file(
                "src/foo.ts",
                "modified",
                600,
                0,
                Some(
                    &std::iter::repeat_n("+line", 600)
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            )],
        );
        assert_eq!(files.len(), 1);
        assert!(files[0].truncated);
        assert_eq!(
            files[0].patch.as_ref().unwrap().lines().count(),
            MAX_PATCH_LINES
        );
    }
}
