// Repository providers: everything host-specific about a connected repository lives behind one
// adapter (GitHub, any other Git host, or a local repository without a remote). Identifying a
// repository only reads what Git already knows locally (the remote URL) and checks local tools, so
// connecting works offline. Pull request operations run only when a change is delivered: the
// default (local) implementation keeps pull requests as records and merges with git; GitHub pushes
// the branch and uses the GitHub CLI.

use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

/** A remote parsed from its URL
 * Fields
    - host: Option<String> - host name, None for a local path
    - owner: Option<String> - owner, organization, or group path
    - name: String - repository name
    - url: String - the URL with any credentials removed
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Remote {
    pub(crate) host: Option<String>,
    pub(crate) owner: Option<String>,
    pub(crate) name: String,
    pub(crate) url: String,
}

/** Remove credentials from a remote URL ("https://user:token@host/x" becomes "https://host/x"),
 * so no secret is ever stored in Crane's state
 * Input
    - url: &str - remote URL
 * Output
    - String
*/
pub(crate) fn sanitize(url: &str) -> String {
    let url = url.trim();
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest
                .split_once('/')
                .map_or((rest, ""), |(authority, path)| (authority, path));
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            // ssh://git@host keeps its user: it names the SSH account, not a secret
            let host = if scheme == "ssh" { authority } else { host };
            if path.is_empty() {
                format!("{scheme}://{host}")
            } else {
                format!("{scheme}://{host}/{path}")
            }
        }
        None => url.to_string(),
    }
}

/** Parse a remote URL into host, owner, and name: https://host/owner/name(.git),
 * ssh://user@host[:port]/owner/name(.git), user@host:owner/name(.git); a local path has no host
 * Input
    - url: &str - remote URL
 * Output
    - Option<Remote>, None when no repository name can be found
*/
pub(crate) fn parse(url: &str) -> Option<Remote> {
    let clean = sanitize(url);
    let (host, path) = if let Some((_, rest)) = clean.split_once("://") {
        if clean.starts_with("file://") {
            (None, rest.to_string())
        } else {
            let (authority, path) = rest.split_once('/')?;
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            let host = host.split(':').next().unwrap_or(host);
            (Some(host.to_ascii_lowercase()), path.to_string())
        }
    } else if let Some((authority, path)) = clean.split_once(':').filter(|(authority, _)| {
        authority.contains('@') || (authority.contains('.') && !authority.contains(['/', '\\']))
    }) {
        // scp-like syntax, but not a Windows drive letter such as C:\repo
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (Some(host.to_ascii_lowercase()), path.to_string())
    } else {
        (None, clean.clone())
    };
    let path = path.replace('\\', "/");
    let segments = path
        .trim_end_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let name = segments.last()?.trim_end_matches(".git").to_string();
    if name.is_empty() {
        return None;
    }
    let owner =
        (host.is_some() && segments.len() >= 2).then(|| segments[..segments.len() - 1].join("/"));
    Some(Remote {
        host,
        owner,
        name,
        url: clean,
    })
}

/** What a delivery asks its provider to open, update, or merge
 * Fields
    - repository: &Path - repository root
    - branch: &str - delivery branch
    - base: &str - branch it merges into
    - head: &str - the checked commit
    - title: &str - pull request title
    - body_file: &Path - pull request body (Markdown)
    - number: Option<u64> - existing pull request number
    - url: Option<&str> - existing pull request link
    - local_number: u64 - number a local pull request gets when it is new
    - url_template: Option<&str> - link template of local pull requests ({number}, {branch})
    - cli: &[String] - the host CLI (program and leading arguments), for providers that use one
*/
pub(crate) struct PullRequest<'a> {
    pub(crate) repository: &'a Path,
    pub(crate) branch: &'a str,
    pub(crate) base: &'a str,
    pub(crate) head: &'a str,
    pub(crate) title: &'a str,
    pub(crate) body_file: &'a Path,
    pub(crate) number: Option<u64>,
    pub(crate) url: Option<&'a str>,
    pub(crate) local_number: u64,
    pub(crate) url_template: Option<&'a str>,
    pub(crate) cli: &'a [String],
}

/** Run a program in a directory
 * Input
    - directory: &Path - working directory
    - program: &str - program
    - args: &[&str] - arguments
 * Output
    - Result<String, String> trimmed stdout, or the program's stderr
*/
fn run_in(directory: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(directory)
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/** Run the host CLI of a pull request
 * Input
    - request: &PullRequest - pull request (its cli and repository)
    - args: &[&str] - arguments after the CLI's own
 * Output
    - Result<String, String>
*/
fn cli(request: &PullRequest, args: &[&str]) -> Result<String, String> {
    let (program, leading) = request
        .cli
        .split_first()
        .ok_or("no host CLI is configured")?;
    let mut all = leading.iter().map(String::as_str).collect::<Vec<_>>();
    all.extend_from_slice(args);
    run_in(request.repository, program, &all)
}

/** A repository host: how its repositories are named and linked, what local tools exist for it,
 * and how delivered changes become pull requests and merges
*/
pub(crate) trait RepositoryProvider {
    /** Return the provider name
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    fn name(&self) -> &'static str;

    /** Check whether a remote belongs to this provider
     * Input
        - remote: &Remote - parsed remote
     * Output
        - bool
    */
    fn recognizes(&self, remote: &Remote) -> bool;

    /** Return the repository's web page
     * Input
        - remote: &Remote - parsed remote
     * Output
        - Option<String>
    */
    fn web_url(&self, remote: &Remote) -> Option<String>;

    /** Return the link template of a pull request ({number}), for delivery when none is configured
     * Input
        - remote: &Remote - parsed remote
     * Output
        - Option<String>
    */
    fn pull_request_url(&self, _remote: &Remote) -> Option<String> {
        None
    }

    /** Report the provider's local capabilities (tools installed here); never contacts the host
     * Input
        - None (uses self)
     * Output
        - Value
    */
    fn capabilities(&self) -> Value {
        json!({"network_checked": false})
    }

    /** Open or update the pull request of a delivery; by default a local record: the number is
     * kept (or the next local number is taken) and the link comes from the template
     * Input
        - request: &PullRequest - pull request
     * Output
        - Result<(u64, String), String> number and link
    */
    fn open_pull_request(&self, request: &PullRequest) -> Result<(u64, String), String> {
        let number = request.number.unwrap_or(request.local_number);
        let url = request.url_template.map_or_else(
            || format!("local://{}/pull/{number}", request.branch),
            |template| {
                template
                    .replace("{number}", &number.to_string())
                    .replace("{branch}", request.branch)
            },
        );
        Ok((number, url))
    }

    /** Merge a delivery's branch into its base and return the merge commit; by default a
     * no-fast-forward git merge in the repository, which must be on the base branch and clean
     * (apart from .crane); a conflict is aborted and reported
     * Input
        - request: &PullRequest - pull request
     * Output
        - Result<String, String> merge commit
    */
    fn merge_pull_request(&self, request: &PullRequest) -> Result<String, String> {
        let repository = request.repository;
        if run_in(repository, "git", &["branch", "--show-current"])? != request.base {
            return Err(format!("check out {} to merge", request.base));
        }
        let dirty = run_in(
            repository,
            "git",
            &["status", "--porcelain", "--untracked-files=no"],
        )?;
        if dirty
            .lines()
            .any(|line| !line.get(3..).unwrap_or_default().starts_with(".crane/"))
        {
            return Err(format!(
                "{} has uncommitted changes; commit or stash them before merging",
                request.base
            ));
        }
        let message = format!(
            "Merge pull request #{} from {}\n\n{}",
            request.number.unwrap_or_default(),
            request.branch,
            request.title
        );
        if let Err(error) = run_in(
            repository,
            "git",
            &[
                "merge",
                "--no-ff",
                "--no-edit",
                "-m",
                &message,
                request.branch,
            ],
        ) {
            let _ = run_in(repository, "git", &["merge", "--abort"]);
            return Err(format!("the merge failed and was aborted: {error}"));
        }
        run_in(repository, "git", &["rev-parse", "HEAD"])
    }
}

/** GitHub (github.com, or GitHub Enterprise hosts named github.* or configured) */
pub(crate) struct GitHub;

impl RepositoryProvider for GitHub {
    /** Return the provider name
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    fn name(&self) -> &'static str {
        "github"
    }

    /** Recognize github.com and github.* hosts with an owner
     * Input
        - remote: &Remote - parsed remote
     * Output
        - bool
    */
    fn recognizes(&self, remote: &Remote) -> bool {
        remote.owner.is_some()
            && remote
                .host
                .as_deref()
                .is_some_and(|host| host == "github.com" || host.starts_with("github."))
    }

    /** Return https://HOST/OWNER/NAME
     * Input
        - remote: &Remote - parsed remote
     * Output
        - Option<String>
    */
    fn web_url(&self, remote: &Remote) -> Option<String> {
        Some(format!(
            "https://{}/{}/{}",
            remote.host.as_deref()?,
            remote.owner.as_deref()?,
            remote.name
        ))
    }

    /** Return https://HOST/OWNER/NAME/pull/{number}
     * Input
        - remote: &Remote - parsed remote
     * Output
        - Option<String>
    */
    fn pull_request_url(&self, remote: &Remote) -> Option<String> {
        self.web_url(remote)
            .map(|url| format!("{url}/pull/{{number}}"))
    }

    /** Report whether the GitHub CLI (used by the github delivery provider) is installed; its
     * authentication is not checked, since that needs the network
     * Input
        - None (uses self)
     * Output
        - Value
    */
    fn capabilities(&self) -> Value {
        let cli = Command::new("gh")
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            });
        json!({"network_checked": false, "gh_cli": cli, "pull_requests": if cli.is_some() { "gh CLI available" } else { "install the GitHub CLI to open pull requests on GitHub" }})
    }

    /** Open or update the pull request on GitHub: push the delivery branch, then create the pull
     * request (or edit the existing one) with the host CLI
     * Input
        - request: &PullRequest - pull request
     * Output
        - Result<(u64, String), String> number and link
    */
    fn open_pull_request(&self, request: &PullRequest) -> Result<(u64, String), String> {
        run_in(
            request.repository,
            "git",
            &["push", "-u", "origin", request.branch],
        )?;
        let body = request.body_file.to_string_lossy().to_string();
        if let Some(number) = request.number {
            cli(
                request,
                &[
                    "pr",
                    "edit",
                    &number.to_string(),
                    "--title",
                    request.title,
                    "--body-file",
                    &body,
                ],
            )?;
            return Ok((number, request.url.unwrap_or_default().to_string()));
        }
        let url = cli(
            request,
            &[
                "pr",
                "create",
                "--base",
                request.base,
                "--head",
                request.branch,
                "--title",
                request.title,
                "--body-file",
                &body,
            ],
        )?;
        let url = url.lines().last().unwrap_or_default().trim().to_string();
        let number = url
            .rsplit('/')
            .next()
            .and_then(|number| number.parse::<u64>().ok())
            .ok_or_else(|| format!("the GitHub CLI did not return a pull request URL: {url}"))?;
        Ok((number, url))
    }

    /** Merge the pull request on GitHub only if its head is still the checked commit (the CLI's
     * --match-head-commit), then read the merge commit from the base branch
     * Input
        - request: &PullRequest - pull request
     * Output
        - Result<String, String> merge commit
    */
    fn merge_pull_request(&self, request: &PullRequest) -> Result<String, String> {
        let number = request
            .number
            .ok_or("the delivery has no pull request number")?
            .to_string();
        cli(
            request,
            &[
                "pr",
                "merge",
                &number,
                "--merge",
                "--match-head-commit",
                request.head,
            ],
        )?;
        run_in(
            request.repository,
            "git",
            &["fetch", "origin", request.base],
        )?;
        run_in(
            request.repository,
            "git",
            &["rev-parse", &format!("origin/{}", request.base)],
        )
    }
}

/** Any other Git host (GitLab, Bitbucket, Gitea, self-hosted) */
pub(crate) struct GitHost;

impl RepositoryProvider for GitHost {
    /** Return the provider name
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    fn name(&self) -> &'static str {
        "git"
    }

    /** Recognize any remote with a host
     * Input
        - remote: &Remote - parsed remote
     * Output
        - bool
    */
    fn recognizes(&self, remote: &Remote) -> bool {
        remote.host.is_some()
    }

    /** Return https://HOST/OWNER/NAME when the owner is known
     * Input
        - remote: &Remote - parsed remote
     * Output
        - Option<String>
    */
    fn web_url(&self, remote: &Remote) -> Option<String> {
        Some(format!(
            "https://{}/{}/{}",
            remote.host.as_deref()?,
            remote.owner.as_deref()?,
            remote.name
        ))
    }
}

/** A repository without a hosted remote (none, or a local path) */
pub(crate) struct Local;

impl RepositoryProvider for Local {
    /** Return the provider name
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    fn name(&self) -> &'static str {
        "local"
    }

    /** Recognize everything (the fallback)
     * Input
        - _remote: &Remote - parsed remote
     * Output
        - bool
    */
    fn recognizes(&self, _remote: &Remote) -> bool {
        true
    }

    /** Return no web page
     * Input
        - _remote: &Remote - parsed remote
     * Output
        - Option<String>
    */
    fn web_url(&self, _remote: &Remote) -> Option<String> {
        None
    }
}

/** Every provider, most specific first */
fn providers() -> Vec<Box<dyn RepositoryProvider>> {
    vec![Box::new(GitHub), Box::new(GitHost), Box::new(Local)]
}

/** Choose the provider of a remote: the one named, or the first that recognizes it
 * Input
    - remote: &Remote - parsed remote
    - forced: Option<&str> - provider named by the user
 * Output
    - Result<Box<dyn RepositoryProvider>, String>
*/
pub(crate) fn select(
    remote: &Remote,
    forced: Option<&str>,
) -> Result<Box<dyn RepositoryProvider>, String> {
    match forced {
        Some(name) => {
            let provider = providers()
                .into_iter()
                .find(|provider| provider.name() == name)
                .ok_or_else(|| format!("unknown provider '{name}'; use github, git, or local"))?;
            if name == "github" && (remote.host.is_none() || remote.owner.is_none()) {
                return Err(
                    "the github provider needs a remote such as https://github.com/OWNER/NAME"
                        .into(),
                );
            }
            Ok(provider)
        }
        None => Ok(providers()
            .into_iter()
            .find(|provider| provider.recognizes(remote))
            .expect("the local provider recognizes everything")),
    }
}

/** Look up a provider by name (for a stored connection)
 * Input
    - name: &str - provider name
 * Output
    - Option<Box<dyn RepositoryProvider>>
*/
pub(crate) fn named(name: &str) -> Option<Box<dyn RepositoryProvider>> {
    providers()
        .into_iter()
        .find(|provider| provider.name() == name)
}
