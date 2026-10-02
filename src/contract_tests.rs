// Contract tests: every active contract clause becomes named, executable verification
// requirements (checkpoint comparison, semantic identity, and scope for preserve; changed, change
// type, and required scope for target), reported apart from the repository's ordinary tests.
// Contract tests are decided by Crane from the code itself; no test file, least of all one an
// agent wrote, can satisfy them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::effects::{run, testing_config, within, workspace_of};
use crate::inventory::graph::is_test_path;
use crate::inventory::outline::language_of;
use crate::inventory::{discover, Options};
use crate::ir::{ContractSet, Postcondition};
use crate::model::{ItemKind, Scope};
use crate::repository::{git, git_raw};
use crate::resolver::{resolve_worktree, Resolution};
use crate::scope::{
    folder_of, folder_prefix, item_features, locate_target, verify_scope, verify_target,
    ScopeContext,
};
use crate::session::{session_ids, ContractSession};
use crate::verify::verify_item;

/** Version of the contract test report layout */
const REPORT_FORMAT: u64 = 1;

/** Default timeout of an ordinary test command, in seconds */
const TEST_TIMEOUT: u64 = 600;

/** One generated contract test
 * Fields
    - id: String - "POLICY:RULE:TARGET:CHECK"
    - policy_id: String - policy
    - rule: &'static str - preserve or target
    - kind: ItemKind - item kind
    - target: String - qualified target
    - check: &'static str - checkpoint, identity, scope, changed, or change_type
    - title: String - what the test asserts, such as "PaymentService.charge preserved"
    - status: &'static str - scheduled, passed, failed, or not_applicable
    - message: String - evidence or the reason for failure
*/
pub(crate) struct ContractTest {
    pub(crate) id: String,
    pub(crate) policy_id: String,
    pub(crate) rule: &'static str,
    pub(crate) kind: ItemKind,
    pub(crate) target: String,
    pub(crate) check: &'static str,
    pub(crate) title: String,
    pub(crate) status: &'static str,
    pub(crate) message: String,
}

impl ContractTest {
    /** Serialize the test
     * Input
        - None (uses self)
     * Output
        - Value JSON object
    */
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "policy_id": self.policy_id,
            "rule": self.rule,
            "kind": self.kind.noun(),
            "target": self.target,
            "check": self.check,
            "title": self.title,
            "status": self.status,
            "message": self.message,
        })
    }
}

/** Generate the contract tests of a contract set without running them: three per preserve clause
 * (checkpoint comparison, semantic identity, scope) and three per target clause (changed, change
 * type, required scope); a malformed policy becomes one failing test
 * Input
    - set: &ContractSet - active contracts
 * Output
    - Vec<ContractTest> with status scheduled (failed for malformed policies)
*/
pub(crate) fn generate(set: &ContractSet) -> Vec<ContractTest> {
    let mut tests = Vec::new();
    for policy in &set.malformed {
        tests.push(ContractTest {
            id: format!("{}:policy", policy.policy_id),
            policy_id: policy.policy_id.clone(),
            rule: "preserve",
            kind: ItemKind::Function,
            target: String::new(),
            check: "policy",
            title: format!("policy {} is well formed", policy.policy_id),
            status: "failed",
            message: policy.message.clone(),
        });
    }
    for (_, contract, clause) in set.clauses() {
        let target = clause.target.clone();
        let scope = clause.scope.name();
        let checks: Vec<(&'static str, String)> = match clause.postcondition() {
            Postcondition::Unchanged => vec![
                ("checkpoint", format!("{target} preserved")),
                ("identity", format!("{target} keeps its semantic identity")),
                ("scope", format!("{target} {scope} scope preserved")),
            ],
            Postcondition::Changed(change_type) => vec![
                ("changed", format!("{target} changed")),
                (
                    "change_type",
                    match change_type {
                        Some(change_type) => format!("{target} {} satisfied", change_type.name()),
                        None => format!("{target} change type (any)"),
                    },
                ),
                ("scope", format!("{target} found within its {scope} scope")),
            ],
        };
        for (check, title) in checks {
            tests.push(ContractTest {
                id: format!(
                    "{}:{}:{}:{check}",
                    contract.policy_id,
                    clause.keyword(),
                    target
                ),
                policy_id: contract.policy_id.clone(),
                rule: clause.keyword(),
                kind: clause.kind,
                target: target.clone(),
                check,
                title,
                status: "scheduled",
                message: String::new(),
            });
        }
    }
    tests
}

/** List the files that changed between a commit and the worktree (tracked changes and untracked
 * files)
 * Input
    - commit: &str - checkpoint commit
 * Output
    - BTreeSet<String> repository-relative paths
*/
fn changed_since(commit: &str) -> BTreeSet<String> {
    let mut paths = git_raw(&["diff", "--name-only", commit, "--"])
        .map(|output| output.lines().map(String::from).collect::<BTreeSet<_>>())
        .unwrap_or_default();
    paths.extend(
        git_raw(&["ls-files", "--others", "--exclude-standard"])
            .map(|output| output.lines().map(String::from).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    paths.retain(|path| !path.starts_with(".crane/"));
    paths
}

/** Run the contract tests of a contract set in the current repository: every check is decided by
 * Crane's own verification of the code against the checkpoint, never by test files
 * Input
    - set: &ContractSet - contracts (from disk, or bound to a session)
    - agent_tests: &BTreeSet<String> - test files agents created or changed, which can never be
      the only change that satisfies a target
 * Output
    - Vec<ContractTest> with status passed, failed, or not_applicable
*/
pub(crate) fn run_contract_tests(
    set: &ContractSet,
    agent_tests: &BTreeSet<String>,
) -> Vec<ContractTest> {
    let mut tests = generate(set);
    let mut context = ScopeContext::new();
    let clauses = set
        .clauses()
        .map(|(_, contract, clause)| (contract, clause))
        .collect::<Vec<_>>();
    for test in tests.iter_mut().filter(|test| test.status == "scheduled") {
        let Some((contract, clause)) = clauses.iter().find(|(contract, clause)| {
            contract.policy_id == test.policy_id
                && clause.keyword() == test.rule
                && clause.target == test.target
        }) else {
            continue;
        };
        let commit = match &contract.checkpoint_sha {
            Ok(commit) => commit.clone(),
            Err(error) => {
                test.status = "failed";
                test.message = error.clone();
                continue;
            }
        };
        let (kind, target, scope) = (clause.kind, clause.target.as_str(), clause.scope);
        let outcome: Result<Option<String>, String> = match test.check {
            "checkpoint" => verify_item(&commit, kind, target)
                .map(|()| Some("equal to the checkpoint version (comments ignored)".into())),
            "identity" => item_features(&mut context, &commit, kind, target).and_then(|(before, after)| {
                let mut differences = Vec::new();
                if before.shape != after.shape {
                    differences.push("structure");
                }
                if before.business != after.business {
                    differences.push("business logic (literals, operators, or calls)");
                }
                if before.complexity != after.complexity {
                    differences.push("complexity (loops or branches)");
                }
                if differences.is_empty() {
                    Ok(Some("resolves once on both sides with the same structure, business logic, and complexity".into()))
                } else {
                    Err(format!("its {} changed", differences.join(" and ")))
                }
            }),
            "scope" if test.rule == "preserve" => match scope {
                Scope::Block => Ok(None),
                _ => verify_scope(&mut context, scope, &commit, kind, target)
                    .map(|()| Some(format!("nothing changed within its {} scope", scope.name()))),
            },
            "changed" => verify_target(&mut context, scope, None, &commit, kind, target).and_then(|()| {
                let region = match scope {
                    Scope::File | Scope::Folder | Scope::All => {
                        let located = locate_target(&mut context, &commit, kind, target)?;
                        let prefix = match scope {
                            Scope::File => located.clone(),
                            Scope::Folder => folder_prefix(&folder_of(&located)),
                            _ => String::new(),
                        };
                        changed_since(&commit)
                            .into_iter()
                            .filter(|path| if scope == Scope::File { *path == prefix } else { path.starts_with(&prefix) })
                            .collect::<Vec<_>>()
                    }
                    _ => Vec::new(),
                };
                if !region.is_empty() && region.iter().all(|path| agent_tests.contains(path)) {
                    Err(format!(
                        "the only changes within its {} scope are agent-authored tests ({}), which cannot satisfy a contract",
                        scope.name(),
                        region.join(", ")
                    ))
                } else {
                    Ok(Some(format!("changed within its {} scope", scope.name())))
                }
            }),
            "change_type" => match clause.postcondition() {
                Postcondition::Changed(Some(change_type)) => verify_target(&mut context, scope, Some(change_type), &commit, kind, target)
                    .map(|()| Some(format!("the change is {}", change_type.name()))),
                _ => Ok(None),
            },
            "scope" => locate_target(&mut context, &commit, kind, target).and_then(|located| {
                match resolve_worktree(kind, target)? {
                    Resolution::Found(_) => Ok(Some(format!("resolves once at the checkpoint ({located}) and in the worktree"))),
                    Resolution::Duplicate(count) => Err(format!("ambiguous in the worktree ({count} matches)")),
                    Resolution::Missing => Err("missing from the worktree".into()),
                    Resolution::Unsupported => Err("the worktree language is unsupported".into()),
                    Resolution::ParseFailure(error) => Err(error),
                }
            }),
            _ => Ok(None),
        };
        match outcome {
            Ok(Some(message)) => {
                test.status = "passed";
                test.message = message;
            }
            Ok(None) => {
                test.status = "not_applicable";
                test.message = match test.check {
                    "scope" => {
                        "block scope covers only the item, which the checkpoint comparison checks"
                            .into()
                    }
                    _ => "the rule requires no particular change type".into(),
                };
            }
            Err(message) => {
                test.status = "failed";
                test.message = message;
            }
        }
    }
    tests
}

/** Find the test files agents created or changed in this repository, from the effects and
 * authorized writes (never reads) journaled by every contract session bound to it
 * Input
    - repository: &str - repository root (git toplevel) the sessions must be bound to
 * Output
    - Result<BTreeSet<String>, String> test file paths
*/
pub(crate) fn agent_authored(repository: &str) -> Result<BTreeSet<String>, String> {
    let normalize = |path: &str| {
        path.replace('\\', "/")
            .trim_end_matches('/')
            .to_ascii_lowercase()
    };
    let mut touched = BTreeSet::new();
    for id in session_ids()? {
        let Ok(Some(session)) = ContractSession::load(&id) else {
            continue;
        };
        if normalize(&session.root_path().to_string_lossy()) != normalize(repository) {
            continue;
        }
        for event in session.events() {
            let effect = &event["effect"]["files"];
            for key in ["added", "modified"] {
                touched.extend(
                    effect[key]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|path| path.as_str().map(String::from)),
                );
            }
            touched.extend(
                effect["renamed"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|pair| pair["to"].as_str().map(String::from)),
            );
            if event["event"] == "pre_tool_use"
                && event["operation"] == "write"
                && matches!(
                    event["decision"].as_str(),
                    Some("allow" | "approval_required")
                )
            {
                touched.extend(
                    event["resources"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|resource| {
                            resource
                                .as_str()
                                .and_then(|text| text.strip_prefix("file:"))
                                .map(String::from)
                        }),
                );
            }
        }
    }
    touched.retain(|path| is_test_path(path));
    Ok(touched)
}

/** Run the ordinary tests, apart from contract tests: organizational tests (present at the
 * checkpoint, or added since by anyone but an agent) always run, with the configured suite command
 * or the per-language command over their files; agent-authored tests run separately and are
 * marked as such; an organizational test an agent changed is reported for review, and one that was
 * deleted fails the run
 * Input
    - checkpoints: &BTreeSet<String> - checkpoint commits of the contracts
    - agent: &BTreeSet<String> - test files agents created or changed
 * Output
    - Result<Value, String> {organizational, agent_authored, findings, status}
*/
pub(crate) fn ordinary_tests(
    checkpoints: &BTreeSet<String>,
    agent: &BTreeSet<String>,
) -> Result<Value, String> {
    let config = testing_config()?;
    let timeout = config["timeout_seconds"].as_u64().unwrap_or(TEST_TIMEOUT);
    let root = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?);
    let inventory = discover(&Options { full: false })?;
    let current = inventory
        .snapshot
        .files
        .iter()
        .enumerate()
        .filter(|(index, _)| inventory.graph.files[*index].test)
        .filter_map(|(_, file)| {
            file.language
                .map(|language| (file.path.clone(), language.name))
        })
        .collect::<BTreeMap<_, _>>();
    let mut at_checkpoint = BTreeSet::new();
    for commit in checkpoints {
        if let Ok(listing) = git(&["ls-tree", "-r", "--name-only", commit]) {
            at_checkpoint.extend(
                listing
                    .lines()
                    .filter(|path| is_test_path(path) && language_of(path).is_some())
                    .map(String::from),
            );
        }
    }
    let mut groups: BTreeMap<(&str, &str), Vec<String>> = BTreeMap::new();
    let mut findings = Vec::new();
    for (path, language) in &current {
        let authored = agent.contains(path) && !at_checkpoint.contains(path);
        let group = if authored {
            "agent_authored"
        } else {
            "organizational"
        };
        groups
            .entry((group, language))
            .or_default()
            .push(path.clone());
        if agent.contains(path) && at_checkpoint.contains(path) {
            findings.push(json!({"kind": "organizational_test_modified_by_agent", "path": path, "blocking": false,
                "message": format!("{path} existed at the checkpoint and was changed by an agent; review the change")}));
        }
    }
    for path in at_checkpoint
        .iter()
        .filter(|path| !current.contains_key(*path))
    {
        findings.push(json!({"kind": "organizational_test_deleted", "path": path, "blocking": true,
            "message": format!("{path} existed at the checkpoint and is gone; organizational tests must keep running")}));
    }
    let mut results: BTreeMap<&str, Vec<Value>> = BTreeMap::from([
        ("organizational", Vec::new()),
        ("agent_authored", Vec::new()),
    ]);
    let mut suites_run = BTreeSet::new();
    for ((group, language), files) in groups {
        let suite = (group == "organizational")
            .then(|| config["suite"][language].as_array())
            .flatten();
        let command = match suite {
            Some(suite) => {
                if !suites_run.insert(language) {
                    continue;
                }
                Some(
                    suite
                        .iter()
                        .filter_map(|part| part.as_str().map(String::from))
                        .collect::<Vec<_>>(),
                )
            }
            None => config["commands"][language].as_array().map(|template| {
                template
                    .iter()
                    .filter_map(|part| part.as_str())
                    .flat_map(|part| {
                        if part == "{files}" {
                            files.clone()
                        } else {
                            vec![part.to_string()]
                        }
                    })
                    .collect::<Vec<_>>()
            }),
        };
        let mut result = match &command {
            Some(command) => {
                let mut result = run(&root, command, timeout);
                result["command"] = json!(command);
                result
            }
            None => {
                json!({"status": "not_configured", "output": format!("no command for {language} in .crane/testing.json")})
            }
        };
        result["language"] = json!(language);
        result["files"] = json!(files);
        results.entry(group).or_default().push(result);
    }
    let failed = results.values().flatten().any(|result| {
        matches!(
            result["status"].as_str(),
            Some("failed" | "timed_out" | "error")
        )
    }) || findings.iter().any(|finding| finding["blocking"] == true);
    Ok(json!({
        "organizational": results["organizational"],
        "agent_authored": results["agent_authored"],
        "findings": findings,
        "status": if failed { "failed" } else { "passed" },
    }))
}

/** Run the contract tests of a session's bound contract inside its worktree
 * Input
    - session: &ContractSession - session
 * Output
    - Result<Vec<ContractTest>, String>
*/
pub(crate) fn for_session(session: &ContractSession) -> Result<Vec<ContractTest>, String> {
    let directory = workspace_of(session)
        .unwrap_or(std::env::current_dir().map_err(|error| error.to_string())?);
    within(&directory, || {
        let repository = git(&["rev-parse", "--show-toplevel"])?;
        let agent = agent_authored(&repository)?;
        Ok(run_contract_tests(session.contracts(), &agent))
    })
}

/** Summarize contract tests for reports and attestations
 * Input
    - tests: &[ContractTest] - tests
 * Output
    - Value {passed, failed, not_applicable, tests}
*/
pub(crate) fn summary(tests: &[ContractTest]) -> Value {
    let count = |status: &str| tests.iter().filter(|test| test.status == status).count();
    json!({
        "passed": count("passed"),
        "failed": count("failed"),
        "not_applicable": count("not_applicable"),
        "scheduled": count("scheduled"),
        "evidence": "Crane verification of the code against the checkpoint; test files never count",
        "tests": tests.iter().map(ContractTest::to_json).collect::<Vec<_>>(),
    })
}

/** Build the full report of crane test-contract
 * Input
    - source: &str - "repository" or "session ID"
    - set: &ContractSet - contracts
    - tests: &[ContractTest] - contract tests
    - ordinary: Option<Value> - ordinary test results, None when skipped
 * Output
    - Value
*/
pub(crate) fn report(
    source: &str,
    set: &ContractSet,
    tests: &[ContractTest],
    ordinary: Option<Value>,
) -> Value {
    let contract_failed = tests.iter().any(|test| test.status == "failed");
    let ordinary_failed = ordinary
        .as_ref()
        .is_some_and(|ordinary| ordinary["status"] == "failed");
    json!({
        "contract_tests_format": REPORT_FORMAT,
        "source": source,
        "contract_version": set.version,
        "contract_tests": summary(tests),
        "ordinary_tests": ordinary,
        "status": if contract_failed || ordinary_failed { "failed" } else if tests.iter().all(|test| test.status == "scheduled") && !tests.is_empty() { "scheduled" } else { "passed" },
    })
}

/** Render the report for a terminal, with contract tests and ordinary tests in separate sections
 * Input
    - report: &Value - report from report()
 * Output
    - String
*/
pub(crate) fn render(report: &Value) -> String {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    let contract = &report["contract_tests"];
    let counts = if contract["scheduled"].as_u64().unwrap_or(0) > 0 {
        format!("{} scheduled, not run", contract["scheduled"])
    } else {
        format!(
            "{} passed, {} failed, {} not applicable",
            contract["passed"], contract["failed"], contract["not_applicable"]
        )
    };
    let mut out = format!("CONTRACT TESTS ({counts}; decided by Crane, never by test files)\n");
    for test in contract["tests"].as_array().into_iter().flatten() {
        let mark = match test["status"].as_str() {
            Some("passed") => "✓",
            Some("failed") => "✗",
            Some("scheduled") => "•",
            _ => "-",
        };
        out.push_str(&format!(
            "{mark} {} [{}]\n",
            text(&test["title"]),
            text(&test["policy_id"])
        ));
        if matches!(test["status"].as_str(), Some("failed" | "not_applicable")) {
            out.push_str(&format!("    {}\n", text(&test["message"])));
        }
    }
    let ordinary = &report["ordinary_tests"];
    if ordinary.is_null() {
        out.push_str("\nORDINARY TESTS\n  not run\n");
    } else {
        out.push_str("\nORDINARY TESTS\n");
        for (group, label) in [
            ("organizational", "organizational"),
            ("agent_authored", "agent-authored"),
        ] {
            let results = ordinary[group].as_array().cloned().unwrap_or_default();
            if results.is_empty() {
                out.push_str(&format!("  {label}: none\n"));
            }
            for result in results {
                let mark = match result["status"].as_str() {
                    Some("passed") => "✓",
                    Some("not_configured") => "-",
                    _ => "✗",
                };
                out.push_str(&format!(
                    "  {mark} {label} {}: {} test files {}\n",
                    text(&result["language"]),
                    result["files"].as_array().map_or(0, Vec::len),
                    text(&result["status"]).replace('_', " ")
                ));
            }
        }
        for finding in ordinary["findings"].as_array().into_iter().flatten() {
            out.push_str(&format!("  ! {}\n", text(&finding["message"])));
        }
    }
    out.push_str(&format!(
        "\nResult: {}\n",
        text(&report["status"]).to_ascii_uppercase()
    ));
    out
}
