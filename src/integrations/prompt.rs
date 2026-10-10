// Interactive prompts for 'crane integrate': text with validation and defaults, numbered choice
// lists (the terminal-portable form of a dropdown), and hidden secret input. When stdin is not a
// terminal (scripts and tests) answers are read line by line from stdin, and secrets are read the
// same way without echo concerns.

use std::io::{self, BufRead, IsTerminal, Write};

/** Reads answers from the terminal or from piped stdin
 * Fields
    - lines: Box<dyn BufRead> - stdin reader
    - interactive: bool - stdin is a terminal
*/
pub(crate) struct Prompter {
    lines: Box<dyn BufRead>,
    interactive: bool,
}

impl Prompter {
    /** Create a prompter on stdin
     * Input
        - None
     * Output
        - Prompter
    */
    pub(crate) fn new() -> Self {
        Self {
            interactive: io::stdin().is_terminal(),
            lines: Box::new(io::BufReader::new(io::stdin())),
        }
    }

    /** Read one answer line
     * Input
        - None (uses self)
     * Output
        - Result<String, String> trimmed answer
        - Error when input ends before an answer
    */
    fn read_line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        let read = self
            .lines
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("input ended before every question was answered".into());
        }
        Ok(line.trim().to_string())
    }

    /** Ask for text, repeating (interactively) until it validates; an empty answer takes the
     * default
     * Input
        - label: &str - question
        - default: Option<&str> - default answer
        - validate: &dyn Fn(&str) -> Result<(), String> - validator
     * Output
        - Result<String, String>
        - Error if a non-interactive answer is invalid or input ends
    */
    pub(crate) fn text(
        &mut self,
        label: &str,
        default: Option<&str>,
        validate: &dyn Fn(&str) -> Result<(), String>,
    ) -> Result<String, String> {
        loop {
            match default {
                Some(default) if !default.is_empty() => print!("{label} [{default}]: "),
                _ => print!("{label}: "),
            }
            io::stdout().flush().ok();
            let mut answer = self.read_line()?;
            if !self.interactive {
                println!();
            }
            if answer.is_empty() {
                answer = default.unwrap_or_default().to_string();
            }
            match validate(&answer) {
                Ok(()) => return Ok(answer),
                Err(error) if self.interactive => println!("  {error}"),
                Err(error) => return Err(format!("{label}: {error}")),
            }
        }
    }

    /** Ask the user to pick one option from a numbered list
     * Input
        - label: &str - question
        - options: &[&str] - options
        - default: usize - index chosen by an empty answer
     * Output
        - Result<usize, String> chosen index
        - Error if a non-interactive answer is not a listed number
    */
    pub(crate) fn choice(
        &mut self,
        label: &str,
        options: &[&str],
        default: usize,
    ) -> Result<usize, String> {
        println!("{label}");
        for (index, option) in options.iter().enumerate() {
            println!(
                "  {}) {option}{}",
                index + 1,
                if index == default { "  (default)" } else { "" }
            );
        }
        let answer = self.text(
            &format!("Select 1-{}", options.len()),
            Some(&(default + 1).to_string()),
            &|answer| match answer.parse::<usize>() {
                Ok(number) if (1..=options.len()).contains(&number) => Ok(()),
                _ => Err(format!("enter a number from 1 to {}", options.len())),
            },
        )?;
        Ok(answer.parse::<usize>().unwrap_or(1) - 1)
    }

    /** Ask for a secret without echoing it when interactive
     * Input
        - label: &str - question
        - optional: bool - an empty answer is allowed
        - validate: &dyn Fn(&str) -> Result<(), String> - validator for non-empty answers
     * Output
        - Result<String, String>
        - Error if the answer is invalid when not interactive, or input ends
    */
    pub(crate) fn secret(
        &mut self,
        label: &str,
        optional: bool,
        validate: &dyn Fn(&str) -> Result<(), String>,
    ) -> Result<String, String> {
        loop {
            let prompt = format!(
                "{label}{} (hidden): ",
                if optional { " [optional]" } else { "" }
            );
            let answer = if self.interactive {
                rpassword::prompt_password(&prompt).map_err(|error| error.to_string())?
            } else {
                print!("{prompt}");
                io::stdout().flush().ok();
                let answer = self.read_line()?;
                println!();
                answer
            };
            let answer = answer.trim().to_string();
            let result = if answer.is_empty() {
                if optional {
                    Ok(())
                } else {
                    Err("a value is required".to_string())
                }
            } else {
                validate(&answer)
            };
            match result {
                Ok(()) => return Ok(answer),
                Err(error) if self.interactive => println!("  {error}"),
                Err(error) => return Err(format!("{label}: {error}")),
            }
        }
    }

    /** Ask a yes or no question
     * Input
        - label: &str - question
        - default: bool - answer for an empty reply
     * Output
        - Result<bool, String>
    */
    pub(crate) fn confirm(&mut self, label: &str, default: bool) -> Result<bool, String> {
        let answer = self.text(
            &format!("{label} ({})", if default { "Y/n" } else { "y/N" }),
            None,
            &|answer| match answer.to_ascii_lowercase().as_str() {
                "" | "y" | "yes" | "n" | "no" => Ok(()),
                _ => Err("answer y or n".into()),
            },
        )?;
        Ok(match answer.to_ascii_lowercase().as_str() {
            "" => default,
            value => value.starts_with('y'),
        })
    }
}
