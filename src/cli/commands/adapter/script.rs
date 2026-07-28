// Renders one `execute` message into a POSIX shell script that the remote
// `/bin/sh` reads from ssh's stdin.
//
// Why a script rather than `ssh host cmd args…`: ssh concatenates the remote
// command and hands it to the remote shell anyway, so the quoting has to be
// exact either way — and this way the environment (which carries
// `${{ secrets.* }}` on ngit-ci runs) never appears in argv on either host.

use std::collections::BTreeMap;

const STDIN_DELIMITER: &str = "PAYGRESS_STDIN";

/// POSIX single-quoting: everything is literal inside `'…'`, and the only
/// character that cannot appear is `'` itself, which is closed, escaped and
/// reopened.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A heredoc body ends at the first line that is exactly the delimiter, so a
/// payload containing that line would truncate the job's stdin. Extending the
/// delimiter until it no longer occurs is cheaper than escaping the payload.
fn free_delimiter(payload: &str) -> String {
    let mut delimiter = STDIN_DELIMITER.to_string();
    while payload.lines().any(|line| line == delimiter) {
        delimiter.push('_');
    }
    delimiter
}

/// `Err` is a malformed request: the caller sees it as a pre-`started`
/// rejection rather than a failed job.
///
/// The job's stdin gains a trailing newline if it lacks one — a heredoc always
/// ends at a line boundary.
pub fn build(
    cmd: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    stdin: &str,
) -> Result<String, String> {
    if cmd.is_empty() {
        return Err("execute message has an empty cmd".to_string());
    }

    let mut script = String::new();
    for (name, value) in env {
        if !valid_env_name(name) {
            return Err(format!("`{}` is not a usable environment name", name));
        }
        script.push_str(&format!("export {}={}\n", name, quote(value)));
    }

    script.push_str(&quote(cmd));
    for arg in args {
        script.push(' ');
        script.push_str(&quote(arg));
    }

    let delimiter = free_delimiter(stdin);
    script.push_str(&format!(" <<'{}'\n", delimiter));
    script.push_str(stdin);
    if !stdin.is_empty() && !stdin.ends_with('\n') {
        script.push('\n');
    }
    script.push_str(&delimiter);
    script.push('\n');

    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn renders_the_ngit_ci_job_shape() {
        let script = build(
            "bash",
            &["-s".to_string()],
            &env_of(&[("CI", "true")]),
            "echo hello\n",
        )
        .expect("builds");
        assert_eq!(
            script,
            "export CI='true'\n'bash' '-s' <<'PAYGRESS_STDIN'\necho hello\nPAYGRESS_STDIN\n"
        );
    }

    #[test]
    fn quotes_hostile_values() {
        let script = build(
            "sh",
            &["-c".to_string(), "echo $(whoami)".to_string()],
            &env_of(&[("TOKEN", "it's; rm -rf /")]),
            "",
        )
        .expect("builds");
        assert!(script.contains(r"export TOKEN='it'\''s; rm -rf /'"));
        assert!(script.contains(r"'sh' '-c' 'echo $(whoami)'"));
    }

    #[test]
    fn extends_the_delimiter_when_stdin_contains_it() {
        let stdin = format!("first\n{}\nlast\n", STDIN_DELIMITER);
        let script = build("bash", &[], &BTreeMap::new(), &stdin).expect("builds");
        assert!(script.contains("<<'PAYGRESS_STDIN_'\n"));
        assert!(script.ends_with("last\nPAYGRESS_STDIN_\n"));
    }

    #[test]
    fn terminates_unterminated_stdin() {
        let script = build("bash", &[], &BTreeMap::new(), "no newline").expect("builds");
        assert!(script.ends_with("no newline\nPAYGRESS_STDIN\n"));
    }

    #[test]
    fn empty_stdin_yields_an_empty_heredoc() {
        let script = build("true", &[], &BTreeMap::new(), "").expect("builds");
        assert_eq!(script, "'true' <<'PAYGRESS_STDIN'\nPAYGRESS_STDIN\n");
    }

    #[test]
    fn rejects_unusable_environment_names() {
        for bad in ["A B", "1A", "A=B", ""] {
            let env = env_of(&[(bad, "x")]);
            assert!(
                build("true", &[], &env, "").is_err(),
                "`{}` should be rejected",
                bad
            );
        }
    }

    #[test]
    fn rejects_an_empty_cmd() {
        assert!(build("", &[], &BTreeMap::new(), "").is_err());
    }

    /// The remote end is `sh` reading its script from ssh's stdin, so the
    /// heredoc is consumed from the same stream the script arrives on. That it
    /// works is load-bearing and not obvious; pin it against a real shell.
    #[test]
    fn a_real_shell_runs_the_rendered_script() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let script = build(
            "bash",
            &["-s".to_string()],
            &env_of(&[("CI", "true")]),
            "echo \"hello $CI\"\nexit 42\n",
        )
        .expect("builds");

        let mut child = Command::new("sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("sh runs");
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(script.as_bytes())
            .expect("writes");
        let output = child.wait_with_output().expect("waits");

        assert_eq!(String::from_utf8_lossy(&output.stdout), "hello true\n");
        assert_eq!(output.status.code(), Some(42), "exit code propagates");
    }
}
