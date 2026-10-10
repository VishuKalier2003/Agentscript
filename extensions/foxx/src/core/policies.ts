// Read-only parsing of AgentScript policy files, enough to list policies and their commands in
// the editor. Crane remains the authority: `crane validate` reports the real syntax errors.

/** A command inside a policy. */
export interface PolicyCommand {
  operation: "preserve" | "target";
  id: string;
  changeType?: string;
  line: number;
}

/** A policy block. */
export interface Policy {
  name: string;
  file: string;
  line: number;
  commands: PolicyCommand[];
}

/**
 * Parse the policies of one `.crane` file, ignoring '#' and '//' comments.
 * @param file repository-relative path
 * @param text file content
 * @returns policies in source order (a malformed tail is ignored)
 */
export function parsePolicies(file: string, text: string): Policy[] {
  const policies: Policy[] = [];
  let current: Policy | undefined;
  text.split(/\r?\n/).forEach((raw, index) => {
    const line = raw.replace(/(#|\/\/).*$/, "").trim();
    if (!line) return;
    const open = /^policy\s+([A-Za-z][A-Za-z0-9_-]*)\s*\{\s*(\})?$/.exec(line);
    if (open) {
      current = { name: open[1], file, line: index + 1, commands: [] };
      policies.push(current);
      if (open[2]) current = undefined;
      return;
    }
    if (line === "}") {
      current = undefined;
      return;
    }
    const command = /^(preserve|target)\s+([A-Z0-9]{8})(?:\s+change_type\s+([a-z_]+))?\s*;$/.exec(line);
    if (command && current) {
      current.commands.push({
        operation: command[1] as "preserve" | "target",
        id: command[2],
        changeType: command[3],
        line: index + 1,
      });
    }
  });
  return policies;
}
