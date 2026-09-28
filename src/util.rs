use std::fmt::Display;
use std::time::{SystemTime, UNIX_EPOCH};

/** Validate a policy or checkpoint name, by rejecting empty values and any character other than
 * ASCII letters, digits, underscore, or hyphen
 * Input
    - value: &str - candidate identifier
 * Output
    - Result<(), String>
    - Error if the identifier is empty or contains an unsafe character
*/
pub(crate) fn validate_identifier(value: &str) -> Result<(), String> {
    // Restrict policy and checkpoint names to portable repository-safe identifiers
    if value.is_empty()
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
    {
        Err(format!("invalid identifier '{value}'"))
    } else {
        Ok(())
    }
}

/** Validate a qualified function target, by first normalizing Rust-style :: separators to dots
 * and then requiring every dot-separated part to be a non-empty run of ASCII letters, digits, or _
 * Input
    - value: &str - target such as Type.method or Type::method
 * Output
    - Result<(), String>
    - Error if any segment is empty or contains an unsupported character
*/
pub(crate) fn validate_function_target(value: &str) -> Result<(), String> {
    // Accept dotted and Rust-style qualified names while rejecting ambiguous syntax
    let normalized = value.replace("::", ".");
    let parts: Vec<_> = normalized.split('.').collect();
    if parts.is_empty()
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
    {
        Err(format!(
            "invalid function target '{value}'; use a qualified name such as Type.method"
        ))
    } else {
        Ok(())
    }
}

/** Read a key-value CLI option such as --function TARGET, by sliding a two-item window across the
 * arguments and returning the value that follows the first matching key
 * Input
    - args: &[String] - command arguments
    - key: &str - option flag to look for
 * Output
    - Option<String>
    - None if the key is absent or has no following value
*/
pub(crate) fn option(args: &[String], key: &str) -> Option<String> {
    // Read simple key-value CLI options without introducing a second parser layer
    args.windows(2)
        .find(|window| window[0] == key)
        .map(|window| window[1].clone())
}

/** Read an option that may be spelled several ways, such as "scope file" (policy style) or
 * "--scope file" (flag style), by finding the first argument equal to any of the keys and
 * returning the argument after it
 * Input
    - args: &[String] - command arguments
    - keys: &[&str] - accepted spellings of the option, the first one used in messages
    - values: &str - accepted values, shown when the value is missing
 * Output
    - Result<Option<String>, String>, None if the option is absent
    - Error if the option is the last argument and so has no value
*/
pub(crate) fn keyword_option(
    args: &[String],
    keys: &[&str],
    values: &str,
) -> Result<Option<String>, String> {
    match args
        .iter()
        .position(|argument| keys.contains(&argument.as_str()))
    {
        None => Ok(None),
        Some(index) => args
            .get(index + 1)
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("{} requires a value: {values}", keys[0])),
    }
}

/** Turn a target into a filesystem-safe name, by mapping each character and replacing anything
 * that is not an ASCII letter or digit with an underscore
 * Input
    - value: &str - text to sanitize
 * Output
    - String with only letters, digits, and underscores
*/
pub(crate) fn sanitize(value: &str) -> String {
    // Derive a filesystem-safe default policy name from a target
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/** Escape a string for Crane's hand-built JSON output, by replacing backslashes first, then double
 * quotes, then newlines with their escaped forms
 * Input
    - value: &str - raw text
 * Output
    - String safe to place inside a JSON string literal
*/
pub(crate) fn escape_json(value: &str) -> String {
    // Escape the characters emitted by Crane's hand-built stable JSON contract
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/** Extract one top-level field from the small checkpoint JSON format, by first finding the quoted
 * key, skipping to the following colon, and then reading either a quoted string (unescaping quotes
 * and backslashes) or a bare value up to the next comma or closing brace
 * Input
    - value: &str - JSON text
    - key: &str - field name to read
 * Output
    - Option<String>
    - None if the key or its value cannot be found
*/
pub(crate) fn json_field(value: &str, key: &str) -> Option<String> {
    // Read the small fixed checkpoint format without accepting arbitrary config syntax
    let needle = format!("\"{key}\"");
    let start = value.find(&needle)?;
    let rest = &value[start + needle.len()..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim_start();
    if let Some(quoted) = value.strip_prefix('"') {
        let end = quoted.find('"')?;
        Some(quoted[..end].replace("\\\"", "\"").replace("\\\\", "\\"))
    } else {
        value
            .split([',', '}'])
            .next()
            .map(|number| number.trim().to_string())
    }
}

/** Get the current Unix timestamp in seconds, by measuring the system clock against UNIX_EPOCH and
 * falling back to 0 if the clock is before the epoch
 * Input
    - None
 * Output
    - u64 seconds since the Unix epoch
*/
pub(crate) fn now_unix() -> u64 {
    // Stamp checkpoint metadata while keeping clock failure non-fatal for this field
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/** Convert any displayable error into Crane's String error type, by calling to_string on it so it
 * can be used with map_err
 * Input
    - error: E - any error implementing Display
 * Output
    - String containing the error message
*/
pub(crate) fn io_error<E: Display>(error: E) -> String {
    // Normalize filesystem and process errors for the CLI error boundary
    error.to_string()
}

/** SHA-256 round constants: the first 32 bits of the fractional parts of the cube roots of the
 * first 64 primes
*/
const SHA256_ROUND: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/** Compute a SHA-256 digest, used as the version of a policy or contract and to identify tool
 * arguments in the execution journal without storing them, by padding the message to whole
 * 64-byte blocks (a 1 bit, zeros, and the 64-bit length), then running the 64-round compression
 * function over each block and finally printing the eight state words as hex
 * Input
    - bytes: &[u8] - data to digest
 * Output
    - String such as "sha256:e3b0c442..."
*/
pub(crate) fn sha256(bytes: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = bytes.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((bytes.len() as u64).wrapping_mul(8)).to_be_bytes());
    for block in message.chunks(64) {
        let mut schedule = [0u32; 64];
        for (word, chunk) in schedule.iter_mut().zip(block.chunks(4)) {
            *word = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for index in 16..64 {
            let early = schedule[index - 15];
            let late = schedule[index - 2];
            let sigma0 = early.rotate_right(7) ^ early.rotate_right(18) ^ (early >> 3);
            let sigma1 = late.rotate_right(17) ^ late.rotate_right(19) ^ (late >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(sigma0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(sigma1);
        }
        let mut working = state;
        for (constant, word) in SHA256_ROUND.iter().zip(schedule) {
            let [a, b, c, d, e, f, g, h] = working;
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let first = h
                .wrapping_add(sum1)
                .wrapping_add(choose)
                .wrapping_add(*constant)
                .wrapping_add(word);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let second = sum0.wrapping_add(majority);
            working = [
                first.wrapping_add(second),
                a,
                b,
                c,
                d.wrapping_add(first),
                e,
                f,
                g,
            ];
        }
        for (value, added) in state.iter_mut().zip(working) {
            *value = value.wrapping_add(added);
        }
    }
    let hex = state
        .iter()
        .map(|word| format!("{word:08x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}
