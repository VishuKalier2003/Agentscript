# Crane Language MVP

## Policy

A policy is a self-contained, non-recursive module.

```crane
policy payment_gateway {
    checkpoint baseline
    preserve --function GatewayService.call
}
```

## Grammar

```text
program       := policy*
policy        := "policy" identifier "{" statement* "}"
statement     := checkpoint_statement | preserve_statement
checkpoint_statement := "checkpoint" identifier
preserve_statement := "preserve" "--function" qualified_name
qualified_name := identifier (("." | "::") identifier)*
```

Blank lines and `#` comments are allowed.

## Semantics

`preserve --function X.Y` (or a single top-level function name) means:

```text
Function(X.Y, checkpoint) == Function(X.Y, current_worktree)
```

The source language is inferred from the protected file's extension. Crane
currently uses tree-sitter grammars for Java, JavaScript, Python, and Rust.
The comparison is strict source-text equality for the extracted function node.
The target is intentionally language-neutral: it is a qualified type/function
name, not a language-specific signature or path. Rust may use its conventional
`Type::method` spelling; the existing dot spelling remains supported for other
languages. Overloads are not disambiguated in this MVP.

A policy's declared name is its stable identifier.

Policies do not import, inherit, recursively invoke, or depend on other policies.

## Checkpoint semantics

A checkpoint is metadata only: it points to an immutable Git commit SHA. Crane
does not copy protected source into `.crane`. Verification reads the baseline
from that commit and the comparison source from the current worktree.

The commit must already exist in the local Git object database. Crane reports a
clear error for a missing commit and never fetches implicitly.

The branch containing that commit may move; the SHA does not.

The policy names its checkpoint explicitly in the MVP. A future version could support a repository default checkpoint.
