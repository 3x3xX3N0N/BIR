//! `bd swarm …` and `bd rules …`.
//!
//! # What is actually built here, and why
//!
//! A swarm is upstream's molecule-of-type-`swarm`: a coordinator bead linked to
//! an epic, orchestrating parallel work across the epic's children. That whole
//! apparatus rides on the molecule lifecycle (`bd mol`), which this port has not
//! built yet, and on a `convoy`-type formula, which [`bd_formula`] parses but
//! does not cook. So most of `swarm` has nothing real to stand on and stays an
//! honest stub — a command that returned ok anyway would report as coverage
//! while doing nothing.
//!
//! Two things *do* have substrate and are implemented:
//!
//! * **`swarm validate <path>`** parses a spec file as a formula and reports
//!   whether it is structurally valid — read-only, creating nothing. A `convoy`
//!   spec validates even though it will not cook yet; the report says so.
//! * **`swarm list`** enumerates the swarm molecules that exist. Nothing in this
//!   port creates one yet, so today the list is empty — but it is a real query
//!   (it would surface a swarm imported from a Go workspace), not a fake ok.
//!
//! `rules audit`/`compact` live here because they are the same shape of thing —
//! reading the workspace's own convention state and reporting or tidying it.
//! `audit` scans `.claude/rules/*.md` for contradictions and merge candidates and
//! is fully real and read-only. `compact` rewrites and deletes those files and
//! upstream refuses to without `--group`/`--auto`/`--dry-run`; this port's
//! flagless `Compact` variant cannot drive it safely, so it stays a stub rather
//! than deleting files by default.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use bd_core::{Issue, IssueFilter, IssueType, MolType};
use bd_formula::FormulaType;
use serde::Serialize;
use serde_json::json;

use crate::cli::{RulesCmd, SwarmCmd};
use crate::commands::stub;
use crate::context::Ctx;

// ---------------------------------------------------------------------------
// bd swarm …
// ---------------------------------------------------------------------------

pub async fn swarm(ctx: &Ctx, cmd: SwarmCmd) -> Result<()> {
    match cmd {
        SwarmCmd::Validate { path } => validate(ctx, &path),
        SwarmCmd::List => list(ctx).await,
        // Upstream `swarm status <epic-id>` computes ready-front progress for a
        // *named* swarm/epic. This port's variant carries no id, and the
        // epic/molecule analysis it would need is not built here yet, so there
        // is nothing honest to compute. Exit 64, not a hollow "no status".
        SwarmCmd::Status => stub("swarm status", ctx),
        // Creating a swarm means creating a `mol_type = swarm` molecule linked to
        // an epic — molecule creation (`bd mol`) is not built yet, and a bare
        // `name` with no epic does not match that model. A wrong-but-plausible
        // create is worse than an honest refusal.
        SwarmCmd::Create { .. } => stub("swarm create", ctx),
    }
}

/// `bd swarm validate <path>` — parse a spec file as a formula and report whether
/// it is valid. Read-only; creates nothing.
///
/// [`bd_formula::parse`] deserializes *and* structurally validates (unique step
/// ids, resolvable edges, no self-edges, a known version), so a clean parse is a
/// genuine "this spec is well-formed". A `convoy`/`aspect`/`expansion` spec is
/// still valid here even though only `workflow` cooks — the report notes it.
fn validate(ctx: &Ctx, path: &Path) -> Result<()> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read swarm spec {}", path.display()))?;

    let formula = match bd_formula::parse(&src) {
        Ok(f) => f,
        // A malformed spec is bad input — exit 1, distinct from an unbuilt
        // command (64) and a backend gap (2). The standard error path renders it
        // (and its --json envelope) the same way every other command's does, so
        // there is no double-printed document under `--json`.
        Err(e) => bail!("{} is not a valid swarm spec: {e}", path.display()),
    };

    let cookable = formula.kind == FormulaType::Workflow;

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "path": path.display().to_string(),
            "valid": true,
            "formula": formula.formula,
            "type": formula.kind.as_str(),
            "description": formula.description,
            "steps": formula.steps.len(),
            "vars": formula.vars.len(),
            "cookable": cookable,
        }));
    }

    ctx.out.line(format!(
        "{} — valid {} spec",
        path.display(),
        formula.kind.as_str()
    ));
    ctx.out.line(format!("  name:  {}", formula.formula));
    if !formula.description.is_empty() {
        ctx.out.line(format!("  about: {}", formula.description));
    }
    ctx.out.line(format!("  steps: {}", formula.steps.len()));
    if !formula.vars.is_empty() {
        ctx.out.line(format!("  vars:  {}", formula.vars.len()));
    }
    if !cookable {
        ctx.out.line(format!(
            "  note:  this port cooks only `workflow` formulas; a `{}` spec parses \
             and validates but `bd cook` does not build it yet.",
            formula.kind.as_str()
        ));
    }
    Ok(())
}

/// `bd swarm list` — the swarm molecules in this workspace.
///
/// [`IssueFilter`] cannot name a molecule *sub*type, so the query filters to
/// molecules and keeps the `swarm` ones in memory. `list_issues` hydrates
/// `mol_type`, so this does not silently drop real swarms. Nothing in this port
/// creates a swarm yet, so the list is empty today — which is a plain fact about
/// the workspace, reported plainly, not a failure.
async fn list(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let filter = IssueFilter {
        issue_type: Some(IssueType::Molecule),
        ..Default::default()
    };
    let swarms: Vec<Issue> = store
        .list_issues(&filter)
        .await?
        .into_iter()
        .filter(|i| i.mol_type == Some(MolType::Swarm))
        .collect();

    if swarms.is_empty() && !ctx.out.is_json() {
        ctx.out.line("No swarms found.");
        return Ok(());
    }
    // The canonical issue renderer keeps `--json` byte-identical to every other
    // listing (the bare `bd_core::Issue` serde shape), which agents already parse.
    ctx.out.issues(&swarms)
}

// ---------------------------------------------------------------------------
// bd rules …
// ---------------------------------------------------------------------------

pub async fn rules(ctx: &Ctx, cmd: RulesCmd) -> Result<()> {
    match cmd {
        RulesCmd::Audit => audit(ctx),
        // `compact` merges rule files and *deletes the sources*. Upstream refuses
        // without `--group`/`--auto` and offers `--dry-run`; this port's flagless
        // `Compact` variant exposes none of those, so it cannot be driven safely.
        // Deleting files by default would be reckless — stub honestly instead.
        RulesCmd::Compact => stub("rules compact", ctx),
    }
}

/// `bd rules audit` — scan `.claude/rules/*.md` for contradictions and merge
/// opportunities. Read-only; touches no database.
///
/// The rules directory is `.claude/rules/` relative to the working directory,
/// matching upstream's default. A missing directory is not an error: it means
/// zero rules.
fn audit(ctx: &Ctx) -> Result<()> {
    // Upstream's default `--threshold`. The port's variant has no flag for it.
    const MERGE_THRESHOLD: f64 = 0.6;
    let dir = ctx.cwd.join(".claude").join("rules");
    let result = run_audit(&dir, MERGE_THRESHOLD)?;

    if ctx.out.is_json() {
        return ctx.out.json_value(&result);
    }
    render_audit(ctx, &dir, &result, MERGE_THRESHOLD);
    Ok(())
}

fn render_audit(ctx: &Ctx, dir: &Path, result: &AuditResult, threshold: f64) {
    ctx.out.line(format!("Rules audit — {}", dir.display()));
    if result.total_rules == 0 {
        ctx.out
            .line("No rule files found (looked for .claude/rules/*.md).");
        return;
    }

    ctx.out.line("");
    ctx.out.line("Summary:");
    ctx.out
        .line(format!("  Total rules:      {}", result.total_rules));
    ctx.out
        .line(format!("  Token estimate:   ~{}", result.token_estimate));
    ctx.out.line(format!(
        "  Contradictions:   {}",
        result.contradictions.len()
    ));
    let merged_rules: usize = result.merge_candidates.iter().map(|m| m.rules.len()).sum();
    if result.merge_candidates.is_empty() {
        ctx.out.line("  Merge candidates: 0");
    } else {
        ctx.out.line(format!(
            "  Merge candidates: {} group(s) ({} rules)",
            result.merge_candidates.len(),
            merged_rules
        ));
    }

    if !result.contradictions.is_empty() {
        ctx.out.line("");
        ctx.out.line("Contradictions:");
        for c in &result.contradictions {
            ctx.out
                .line(format!("  {} vs {}: {}", c.rule_a, c.rule_b, c.tension));
        }
    }

    if !result.merge_candidates.is_empty() {
        ctx.out.line("");
        ctx.out
            .line(format!("Merge candidates (similarity > {threshold:.2}):"));
        for (i, m) in result.merge_candidates.iter().enumerate() {
            ctx.out.line(format!(
                "  Group {} — \"{}\" (score {:.2})",
                i + 1,
                m.group_label,
                m.score
            ));
            for r in &m.rules {
                ctx.out.line(format!("    -> {r}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rules audit — the pure engine
//
// A faithful port of upstream `cmd/bd/rules.go`. It does no I/O beyond reading
// the rule files, so all of the logic below is testable with strings. The one
// deliberate divergence: this port has no `regex` dependency available, so the
// directive/heading matchers are hand-rolled — and the maps are ordered
// (`BTreeMap`) so a contradiction report is deterministic rather than depending
// on Go's randomized map iteration.
// ---------------------------------------------------------------------------

/// A parsed `.claude/rules/*.md` file.
#[derive(Debug, Clone, Serialize)]
struct RuleFile {
    name: String,
    title: String,
    do_lines: Vec<String>,
    dont_lines: Vec<String>,
    keywords: Vec<String>,
    tokens: usize,
}

/// A tension between two rules — one's `Do` opposes another's `Don't`, or their
/// `Do`s use antonyms over shared scope.
#[derive(Debug, Clone, Serialize)]
struct Contradiction {
    rule_a: String,
    rule_b: String,
    tension: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    do_line_a: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    dont_line_b: String,
    scope_score: f64,
}

/// A group of rules similar enough to fold into one.
#[derive(Debug, Clone, Serialize)]
struct MergeCandidate {
    group_label: String,
    rules: Vec<String>,
    score: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
struct AuditResult {
    total_rules: usize,
    token_estimate: usize,
    contradictions: Vec<Contradiction>,
    merge_candidates: Vec<MergeCandidate>,
    rules: Vec<RuleFile>,
}

/// Top-level orchestrator. A missing directory means zero rules, not an error.
fn run_audit(dir: &Path, threshold: f64) -> Result<AuditResult> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AuditResult::default()),
        Err(e) => return Err(anyhow!("cannot read rules directory {}: {e}", dir.display())),
    };

    // Sorted for a deterministic report, independent of readdir order.
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    paths.sort();

    let mut rules = Vec::new();
    let mut token_estimate = 0;
    for path in &paths {
        // A file we cannot read is skipped, not fatal — one bad rule should not
        // sink the audit of the rest.
        if let Ok(rf) = parse_rule_file(path) {
            token_estimate += rf.tokens;
            rules.push(rf);
        }
    }

    let mut result = AuditResult {
        total_rules: rules.len(),
        token_estimate,
        ..Default::default()
    };
    // Contradictions and merges are pairwise: they need at least two rules.
    if rules.len() >= 2 {
        result.contradictions = detect_contradictions(&rules, 0.3);
        result.merge_candidates = find_merge_candidates(&rules, threshold);
    }
    result.rules = rules;
    Ok(result)
}

fn parse_rule_file(path: &Path) -> Result<RuleFile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read rule file {}", path.display()))?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    let title = heading_title(&content).unwrap_or_else(|| name.clone());
    let (do_lines, dont_lines) = extract_directives(&content);

    // Keyword scope comes from the directives when there are any, else the whole
    // body — otherwise a rule written as prose has no scope and never matches.
    let keywords = if do_lines.is_empty() && dont_lines.is_empty() {
        extract_keywords(std::slice::from_ref(&content))
    } else {
        let mut all = do_lines.clone();
        all.extend(dont_lines.iter().cloned());
        extract_keywords(&all)
    };

    Ok(RuleFile {
        name,
        title,
        do_lines,
        dont_lines,
        keywords,
        // A rough estimate, ~4 chars per token, matching upstream.
        tokens: content.len() / 4,
    })
}

/// The first `# Heading` (single hash + space), or `None`. `## Sub` is ignored,
/// matching upstream's `^#\s+` anchor.
fn heading_title(content: &str) -> Option<String> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix('#')
            && rest.starts_with(char::is_whitespace)
        {
            let t = rest.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Pull `Do`/`Don't` directive lines out of a rule body.
///
/// A `**Do:**`/`**Don't:**` header opens a block; subsequent bullet or plain
/// lines extend it until a blank line, heading, or bold header closes it. `Don't`
/// is tested first because it contains `Do`.
fn extract_directives(content: &str) -> (Vec<String>, Vec<String>) {
    let mut do_lines = Vec::new();
    let mut dont_lines = Vec::new();
    // 0 = none, 1 = do, 2 = dont.
    let mut block = 0u8;

    for line in content.lines() {
        let trimmed = line.trim();

        if let Some(text) = strip_directive(line, Directive::Dont) {
            block = 2;
            push_nonempty(&mut dont_lines, text.trim());
            continue;
        }
        if let Some(text) = strip_directive(line, Directive::Do) {
            block = 1;
            push_nonempty(&mut do_lines, text.trim());
            continue;
        }

        if block == 0 {
            continue;
        }

        let is_bullet =
            trimmed.starts_with('-') || (trimmed.starts_with('*') && !trimmed.starts_with("**"));
        if is_bullet {
            let text = trimmed.trim_start_matches(['-', '*', ' ']);
            let target = if block == 1 { &mut do_lines } else { &mut dont_lines };
            push_nonempty(target, text);
        } else if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("**") {
            block = 0;
        } else {
            let target = if block == 1 { &mut do_lines } else { &mut dont_lines };
            target.push(trimmed.to_string());
        }
    }
    (do_lines, dont_lines)
}

fn push_nonempty(v: &mut Vec<String>, s: &str) {
    if !s.is_empty() {
        v.push(s.to_string());
    }
}

#[derive(Clone, Copy)]
enum Directive {
    Do,
    Dont,
}

/// If `line` opens the given directive block, return the inline text after the
/// header. Matches `^\*\*Do:?\*\*:?\s*` / `^\*\*Don'?t:?\*\*:?\s*`,
/// case-insensitively, at column zero (as upstream's anchored regex does).
fn strip_directive(line: &str, kind: Directive) -> Option<&str> {
    let r = line.strip_prefix("**")?;
    let r = match kind {
        Directive::Do => ci_strip(r, "do")?,
        Directive::Dont => {
            let r = ci_strip(r, "don")?;
            let r = r.strip_prefix('\'').unwrap_or(r);
            ci_strip(r, "t")?
        }
    };
    let r = r.strip_prefix(':').unwrap_or(r);
    let r = r.strip_prefix("**")?;
    let r = r.strip_prefix(':').unwrap_or(r);
    Some(r.trim_start())
}

/// Case-insensitive prefix strip, ASCII-only and char-boundary safe.
fn ci_strip<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Lowercased, de-duplicated, sorted keywords — stop words and single chars out.
fn extract_keywords(lines: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut keywords = Vec::new();
    for line in lines {
        for w in tokenize(line) {
            let w = w.to_lowercase();
            if w.len() < 2 || is_stop_word(&w) {
                continue;
            }
            if seen.insert(w.clone()) {
                keywords.push(w);
            }
        }
    }
    keywords.sort();
    keywords
}

/// Split into words on anything that is not a letter, digit, or apostrophe.
fn tokenize(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
}

/// Keyword-set overlap: |A ∩ B| / |A ∪ B|.
fn jaccard(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let set_a: HashSet<&str> = a.iter().map(String::as_str).collect();
    let set_b: HashSet<&str> = b.iter().map(String::as_str).collect();
    let intersection = set_a.iter().filter(|w| set_b.contains(*w)).count();
    let union = set_a.union(&set_b).count();
    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
}

/// Lowercase action word → the first line it appeared on. Ordered so results are
/// deterministic.
fn action_words(lines: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in lines {
        for w in tokenize(line) {
            let w = w.to_lowercase();
            if w.len() >= 2 && !is_stop_word(&w) {
                out.entry(w).or_insert_with(|| line.clone());
            }
        }
    }
    out
}

fn detect_contradictions(rules: &[RuleFile], scope_threshold: f64) -> Vec<Contradiction> {
    let mut reports = Vec::new();
    for i in 0..rules.len() {
        for j in (i + 1)..rules.len() {
            let (a, b) = (&rules[i], &rules[j]);
            let score = jaccard(&a.keywords, &b.keywords);
            if score < scope_threshold {
                continue;
            }
            // A's `Do` verb appears in B's `Don't`.
            if let Some(c) = direct_contradiction(a, b, score) {
                reports.push(c);
                continue;
            }
            // The reverse: B's `Do` verb in A's `Don't`. Report with A/B in
            // declaration order regardless of which direction found it.
            if let Some(mut c) = direct_contradiction(b, a, score) {
                c.rule_a = format!("{}.md", a.name);
                c.rule_b = format!("{}.md", b.name);
                reports.push(c);
                continue;
            }
            // Antonyms in the two `Do` sets over shared scope.
            if let Some(c) = antonym_contradiction(a, b, score) {
                reports.push(c);
            }
        }
    }
    reports
}

fn direct_contradiction(a: &RuleFile, b: &RuleFile, score: f64) -> Option<Contradiction> {
    let a_do = action_words(&a.do_lines);
    let b_dont = action_words(&b.dont_lines);
    for (word, do_line) in &a_do {
        if let Some(dont_line) = b_dont.get(word) {
            return Some(contradiction(a, b, do_line, dont_line, score));
        }
    }
    None
}

fn antonym_contradiction(a: &RuleFile, b: &RuleFile, score: f64) -> Option<Contradiction> {
    let a_do = action_words(&a.do_lines);
    let b_do = action_words(&b.do_lines);
    for (word, line_a) in &a_do {
        for ant in antonyms(word) {
            if let Some(line_b) = b_do.get(*ant) {
                return Some(contradiction(a, b, line_a, line_b, score));
            }
        }
    }
    None
}

fn contradiction(
    a: &RuleFile,
    b: &RuleFile,
    line_a: &str,
    line_b: &str,
    score: f64,
) -> Contradiction {
    let tension = truncate(
        &format!("\"{}\" vs \"{}\"", summarize(line_a), summarize(line_b)),
        60,
        57,
    );
    Contradiction {
        rule_a: format!("{}.md", a.name),
        rule_b: format!("{}.md", b.name),
        tension,
        do_line_a: line_a.to_string(),
        dont_line_b: line_b.to_string(),
        scope_score: score,
    }
}

fn summarize(line: &str) -> String {
    truncate(line, 40, 37)
}

/// Truncate to `max` chars, keeping `keep` and appending `...`. Char-safe.
fn truncate(s: &str, max: usize, keep: usize) -> String {
    if s.chars().count() > max {
        let head: String = s.chars().take(keep).collect();
        format!("{head}...")
    } else {
        s.to_string()
    }
}

/// Single-linkage clustering over keyword similarity: any two rules within
/// `threshold` land in the same group; groups of two or more become candidates.
fn find_merge_candidates(rules: &[RuleFile], threshold: f64) -> Vec<MergeCandidate> {
    let n = rules.len();
    if n < 2 {
        return Vec::new();
    }

    let mut edges = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            if jaccard(&rules[i].keywords, &rules[j].keywords) >= threshold {
                edges.push((i, j));
            }
        }
    }
    if edges.is_empty() {
        return Vec::new();
    }

    let mut parent: Vec<usize> = (0..n).collect();
    for (i, j) in edges {
        let (pi, pj) = (uf_find(&mut parent, i), uf_find(&mut parent, j));
        if pi != pj {
            parent[pi] = pj;
        }
    }

    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        let root = uf_find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }

    let mut candidates = Vec::new();
    for members in groups.values() {
        if members.len() < 2 {
            continue;
        }
        // Average pairwise similarity within the group.
        let mut total = 0.0;
        let mut pairs = 0;
        for mi in 0..members.len() {
            for mj in (mi + 1)..members.len() {
                total += jaccard(&rules[members[mi]].keywords, &rules[members[mj]].keywords);
                pairs += 1;
            }
        }
        let avg = if pairs > 0 { total / pairs as f64 } else { 0.0 };

        let mut names: Vec<String> = members.iter().map(|&i| format!("{}.md", rules[i].name)).collect();
        names.sort();

        candidates.push(MergeCandidate {
            group_label: group_label(rules, members),
            rules: names,
            score: round2(avg),
        });
    }
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    candidates
}

fn uf_find(parent: &mut [usize], x: usize) -> usize {
    if parent[x] != x {
        let px = parent[x];
        let root = uf_find(parent, px);
        parent[x] = root;
        root
    } else {
        x
    }
}

/// The most frequent keyword across a group, ties broken lexicographically;
/// `"rules"` if the group has no keywords at all.
fn group_label(rules: &[RuleFile], indices: &[usize]) -> String {
    let mut freq: BTreeMap<&str, i32> = BTreeMap::new();
    for &i in indices {
        for kw in &rules[i].keywords {
            *freq.entry(kw.as_str()).or_insert(0) += 1;
        }
    }
    let mut best = "rules";
    let mut best_count = 0;
    for (w, c) in &freq {
        if *c > best_count || (*c == best_count && *w < best) {
            best = w;
            best_count = *c;
        }
    }
    best.to_string()
}

fn round2(f: f64) -> f64 {
    ((f * 100.0 + 0.5) as i64) as f64 / 100.0
}

fn is_stop_word(w: &str) -> bool {
    matches!(
        w,
        "the" | "a" | "is" | "to" | "for" | "and" | "or" | "in" | "of" | "it"
            | "that" | "this" | "with" | "be" | "not" | "do" | "don't" | "use" | "when" | "before"
            | "after" | "should" | "must" | "always" | "never" | "an" | "are" | "as" | "at" | "by"
            | "from" | "has" | "have" | "if" | "on" | "was" | "were" | "will" | "you" | "your"
    )
}

fn antonyms(word: &str) -> &'static [&'static str] {
    match word {
        "block" => &["proceed", "parallel"],
        "proceed" => &["block"],
        "parallel" => &["block"],
        "verbose" => &["minimize", "concise"],
        "minimize" => &["verbose"],
        "concise" => &["verbose"],
        "spawn" => &["reuse"],
        "reuse" => &["spawn"],
        "wait" => &["skip"],
        "skip" => &["wait"],
        "log" => &["suppress"],
        "suppress" => &["log"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, do_lines: &[&str], dont_lines: &[&str]) -> RuleFile {
        let do_v: Vec<String> = do_lines.iter().map(|s| s.to_string()).collect();
        let dont_v: Vec<String> = dont_lines.iter().map(|s| s.to_string()).collect();
        let mut all = do_v.clone();
        all.extend(dont_v.iter().cloned());
        let keywords = extract_keywords(&all);
        RuleFile {
            name: name.to_string(),
            title: name.to_string(),
            do_lines: do_v,
            dont_lines: dont_v,
            keywords,
            tokens: 0,
        }
    }

    #[test]
    fn heading_takes_the_first_h1_and_ignores_h2() {
        assert_eq!(heading_title("## Sub\n# Real\nbody").as_deref(), Some("Real"));
        assert_eq!(heading_title("#nospace\n").as_deref(), None);
        assert_eq!(heading_title("no heading here").as_deref(), None);
    }

    #[test]
    fn dont_is_matched_before_do() {
        // "**Don't**" must not be read as a "Do" block just because it contains "Do".
        let body = "**Do:** spawn agents\n**Don't:** block the queue\n";
        let (do_lines, dont_lines) = extract_directives(body);
        assert_eq!(do_lines, vec!["spawn agents"]);
        assert_eq!(dont_lines, vec!["block the queue"]);
    }

    #[test]
    fn a_directive_block_absorbs_following_bullets() {
        let body = "**Do:**\n- first thing\n- second thing\n\n**Don't:** stop\n";
        let (do_lines, dont_lines) = extract_directives(body);
        assert_eq!(do_lines, vec!["first thing", "second thing"]);
        assert_eq!(dont_lines, vec!["stop"]);
    }

    #[test]
    fn keywords_drop_stop_words_and_sort() {
        let kw = extract_keywords(&["Do NOT spawn a New Agent".to_string()]);
        // "do", "not", "a" are stop words; the rest lowercased, sorted, unique.
        assert_eq!(kw, vec!["agent", "new", "spawn"]);
    }

    #[test]
    fn jaccard_is_intersection_over_union() {
        let a = vec!["agent".to_string(), "spawn".to_string(), "task".to_string()];
        let b = vec!["agent".to_string(), "reuse".to_string(), "task".to_string()];
        // {agent, task} shared, union of 4.
        assert!((jaccard(&a, &b) - 0.5).abs() < 1e-9);
        assert_eq!(jaccard(&[], &[]), 0.0);
    }

    #[test]
    fn antonym_do_lines_over_shared_scope_contradict() {
        let a = rule("spawn", &["spawn a new agent per task"], &[]);
        let b = rule("reuse", &["reuse the existing agent per task"], &[]);
        let reports = detect_contradictions(&[a, b], 0.3);
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].rule_a, "spawn.md");
        assert_eq!(reports[0].rule_b, "reuse.md");
    }

    #[test]
    fn direct_do_vs_dont_contradiction_is_found() {
        let a = rule("a", &["log every retry attempt"], &[]);
        let b = rule("b", &[], &["log every retry attempt"]);
        let reports = detect_contradictions(&[a, b], 0.3);
        assert_eq!(reports.len(), 1, "{reports:?}");
    }

    #[test]
    fn unrelated_rules_do_not_contradict_or_merge() {
        let a = rule("a", &["spawn workers quickly"], &[]);
        let b = rule("b", &["document the release checklist"], &[]);
        assert!(detect_contradictions(&[a.clone(), b.clone()], 0.3).is_empty());
        assert!(find_merge_candidates(&[a, b], 0.6).is_empty());
    }

    #[test]
    fn similar_rules_become_a_merge_candidate() {
        let a = rule("a", &["review the pull request carefully"], &[]);
        let b = rule("b", &["review the pull request thoroughly"], &[]);
        let cands = find_merge_candidates(&[a, b], 0.5);
        assert_eq!(cands.len(), 1, "{cands:?}");
        assert_eq!(cands[0].rules, vec!["a.md", "b.md"]);
    }
}
