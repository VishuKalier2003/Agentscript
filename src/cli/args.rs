// Argument parsing for Crane's keyword-style commands, such as
// "crane protect FILE policy NAME checkpoint NAME start-line 3 end-line 9": positional words come
// first, then KEY VALUE pairs; each key may also be written with a leading "--", and "_" and "-"
// are interchangeable in key names.

use std::collections::BTreeMap;

/** Parsed command arguments
 * Fields
    - positional: Vec<String> - words before the first recognized key or flag
    - values: BTreeMap<String, String> - KEY VALUE options by canonical key
    - flags: Vec<String> - recognized flags present (canonical names)
*/
#[derive(Debug, Clone, Default)]
pub(crate) struct Args {
    pub(crate) positional: Vec<String>,
    pub(crate) values: BTreeMap<String, String>,
    pub(crate) flags: Vec<String>,
}

/** Reduce a key as written to its canonical form: no leading dashes, lowercase, "_" as "-"
 * Input
    - word: &str - argument
 * Output
    - String
*/
fn canonical(word: &str) -> String {
    word.trim_start_matches('-')
        .to_ascii_lowercase()
        .replace('_', "-")
}

impl Args {
    /** Parse arguments against the keys and flags a command accepts, rejecting unknown options,
     * repeated keys, keys without a value, and positional words after the options
     * Input
        - args: &[String] - arguments after the command words
        - keys: &[&str] - canonical keys that take a value, such as "start-line"
        - flags: &[&str] - canonical flags that take no value, such as "json"
     * Output
        - Result<Args, String>
        - Error naming the offending word
    */
    pub(crate) fn parse(args: &[String], keys: &[&str], flags: &[&str]) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut index = 0;
        while index < args.len() {
            let word = &args[index];
            let key = canonical(word);
            if flags.contains(&key.as_str()) {
                parsed.flags.push(key);
                index += 1;
            } else if keys.contains(&key.as_str()) {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("'{word}' needs a value"))?;
                if parsed.values.insert(key.clone(), value.clone()).is_some() {
                    return Err(format!("'{word}' is given twice"));
                }
                index += 2;
            } else if word.starts_with("--") {
                return Err(format!("unknown option '{word}'"));
            } else if parsed.values.is_empty() && parsed.flags.is_empty() {
                parsed.positional.push(word.clone());
                index += 1;
            } else {
                return Err(format!("unexpected '{word}'"));
            }
        }
        Ok(parsed)
    }

    /** Return a value option
     * Input
        - key: &str - canonical key
     * Output
        - Option<String>
    */
    pub(crate) fn value(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned()
    }

    /** Return a value option parsed as a positive line number
     * Input
        - key: &str - canonical key
     * Output
        - Result<Option<usize>, String>
        - Error if the value is not a positive integer
    */
    pub(crate) fn line(&self, key: &str) -> Result<Option<usize>, String> {
        self.value(key)
            .map(|value| {
                value
                    .parse::<usize>()
                    .ok()
                    .filter(|number| *number > 0)
                    .ok_or_else(|| format!("{key} must be a positive line number, got '{value}'"))
            })
            .transpose()
    }

    /** Report whether a flag is present
     * Input
        - flag: &str - canonical flag
     * Output
        - bool
    */
    pub(crate) fn flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|present| present == flag)
    }

    /** Require exactly a number of positional words
     * Input
        - count: usize - expected count
        - usage: &str - usage text for the error
     * Output
        - Result<(), String>
        - Error with the usage when the count differs
    */
    pub(crate) fn expect_positional(&self, count: usize, usage: &str) -> Result<(), String> {
        if self.positional.len() == count {
            Ok(())
        } else {
            Err(format!("usage: {usage}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Args;

    /** Convert string slices to owned arguments
     * Input
        - words: &[&str] - arguments
     * Output
        - Vec<String>
    */
    fn owned(words: &[&str]) -> Vec<String> {
        words.iter().map(std::string::ToString::to_string).collect()
    }

    /** Check keyword parsing, dashed spellings, flags, and errors
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn parses_keyword_arguments() {
        let args = Args::parse(
            &owned(&[
                "pay.py",
                "policy",
                "payments",
                "--start_line",
                "3",
                "end-line",
                "9",
                "--json",
            ]),
            &["policy", "start-line", "end-line"],
            &["json"],
        )
        .unwrap();
        assert_eq!(args.positional, vec!["pay.py"]);
        assert_eq!(args.value("policy").as_deref(), Some("payments"));
        assert_eq!(args.line("start-line").unwrap(), Some(3));
        assert!(args.flag("json"));
        assert!(Args::parse(&owned(&["a", "policy"]), &["policy"], &[]).is_err());
        assert!(Args::parse(&owned(&["--bogus"]), &[], &[]).is_err());
        assert!(Args::parse(&owned(&["policy", "a", "policy", "b"]), &["policy"], &[]).is_err());
        let bad = Args::parse(&owned(&["start-line", "0"]), &["start-line"], &[]).unwrap();
        assert!(bad.line("start-line").is_err());
    }
}
