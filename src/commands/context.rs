use crate::ir::{compile, ContractSet, Permission, Postcondition};
use crate::model::ChangeType;
use crate::repository::ensure_initialized;

/** Print policy context for an agent session, by first confirming Crane is initialized, then
 * compiling every policy (failing on the first malformed one), and finally printing the context
 * Input
    - None
 * Output
    - Result<(), String>
    - Error if not initialized or any policy fails to parse
*/
pub(crate) fn run() -> Result<(), String> {
    // Emit deterministic policy context that an agent can consume at session start
    ensure_initialized()?;
    let set = compile()?;
    if let Some(policy) = set.malformed.first() {
        return Err(policy.message.clone());
    }
    print!("{}", render(&set));
    Ok(())
}

/** Render the model-readable context of a contract set: the stable CRANE_CONTEXT_V1 listing of
 * every policy's checkpoint and rules, followed by the contract version and a plain summary of
 * what must not and what must change; it holds no runtime authority material, the model only
 * reads it while Crane enforces the contract on its own
 * Input
    - set: &ContractSet - compiled contracts, from disk or from a session
 * Output
    - String ending in a newline
*/
pub(crate) fn render(set: &ContractSet) -> String {
    let mut output = String::from("CRANE_CONTEXT_V1\n");
    let mut preserved = Vec::new();
    let mut required = Vec::new();
    for contract in &set.contracts {
        output.push_str(&format!(
            "\npolicy_id: {}\ncheckpoint: {}\n",
            contract.policy_id, contract.checkpoint
        ));
        for clause in &contract.clauses {
            let (kind, target, scope) = (clause.kind.noun(), &clause.target, clause.scope.name());
            output.push_str(&format!(
                "rule: {}\nkind: {kind}\ntarget: {target}\nscope: {scope}\n",
                clause.keyword()
            ));
            if let Postcondition::Changed(change_type) = clause.postcondition {
                // Targets tell the agent which change its task must make
                let change_type = change_type.map_or("any", ChangeType::name);
                output.push_str(&format!("change_type: {change_type}\n"));
                required.push(format!(
                    "- {kind} {target} (scope {scope}; change_type {change_type}; policy {})",
                    contract.policy_id
                ));
            }
            if clause.permission == Permission::DenyWrite {
                preserved.push(format!(
                    "- {kind} {target} (scope {scope}; policy {})",
                    contract.policy_id
                ));
            }
        }
    }
    output.push_str("\nverification: crane check --agent\n");
    output.push_str(&format!("contract_version: {}\n", set.version));
    output.push_str(
        "\nACTIVE CONTRACT (informational; Crane enforces it independently of this text)\n",
    );
    for (title, lines) in [("Do not modify:", preserved), ("Must modify:", required)] {
        if !lines.is_empty() {
            output.push_str(&format!("{title}\n{}\n", lines.join("\n")));
        }
    }
    if !set.malformed.is_empty() {
        output.push_str("Unavailable policies (a human must repair them; mutating tools are denied until then):\n");
        for policy in &set.malformed {
            output.push_str(&format!("- {}\n", policy.policy_id));
        }
    }
    output
}
