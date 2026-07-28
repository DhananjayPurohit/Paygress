// Loom execution-adapter wire contract, as vendored by ngit-ci in
// `docs/execution-adapter-protocol.md`: line-delimited JSON over a Unix
// socket, one `execute` per connection, adapter closes after a terminal
// message.
//
// Field names are the wire format and are not ours to tidy: `exitCode` is
// camelCase while everything else is not.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Request {
    Execute(Execute),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Execute {
    /// Opaque to us; the caller uses it to correlate its own logs.
    #[serde(default)]
    pub identifier: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Written to the job's stdin in full, then stdin is closed.
    #[serde(default)]
    pub stdin: String,
    /// BTreeMap so the generated remote script is deterministic, which is what
    /// makes `script.rs`'s tests meaningful.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    // `artifact_dir` (ngit-ci's artifact-streaming extension) is deliberately
    // absent: serde drops unknown fields, and the contract lets an adapter that
    // does not stream artifacts ignore it — at the cost of `upload-artifact`.
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Response {
    /// Capacity acquired and rejection can no longer happen. Never a claim
    /// that the command is running.
    Started,
    Stdout {
        data: String,
    },
    Stderr {
        data: String,
    },
    Completed {
        #[serde(rename = "exitCode")]
        exit_code: i32,
        /// Whole seconds, floored.
        duration: u64,
    },
    Error {
        error: String,
    },
}

impl Response {
    /// One message per line. `serde_json` escapes newlines inside strings, so
    /// job output can never break framing.
    pub fn to_line(&self) -> String {
        // Every variant is plain strings and integers, so this cannot fail;
        // the fallback is still valid framing rather than a panic.
        let mut line = serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"type":"error","error":"unserializable response"}"#.to_string()
        });
        line.push('\n');
        line
    }
}

pub fn parse_request(line: &str) -> Result<Execute, String> {
    match serde_json::from_str::<Request>(line) {
        Ok(Request::Execute(e)) => Ok(e),
        Err(e) => Err(format!("malformed request: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_execute_message() {
        let line = r#"{"type":"execute","identifier":"job-1","cmd":"bash","args":["-s"],"stdin":"echo hi\n","env":{"A":"1"}}"#;
        let e = parse_request(line).expect("parses");
        assert_eq!(e.identifier, "job-1");
        assert_eq!(e.cmd, "bash");
        assert_eq!(e.args, vec!["-s"]);
        assert_eq!(e.stdin, "echo hi\n");
        assert_eq!(e.env.get("A").map(String::as_str), Some("1"));
    }

    #[test]
    fn optional_fields_default() {
        let e = parse_request(r#"{"type":"execute","cmd":"true"}"#).expect("parses");
        assert!(e.args.is_empty());
        assert!(e.stdin.is_empty());
        assert!(e.env.is_empty());
    }

    #[test]
    fn unknown_fields_do_not_reject_a_request() {
        // ngit-ci sends `artifact_dir`; adapters that ignore it must still run.
        parse_request(r#"{"type":"execute","cmd":"true","artifact_dir":"/artifacts"}"#)
            .expect("parses");
    }

    #[test]
    fn unknown_message_types_are_rejected() {
        assert!(parse_request(r#"{"type":"cancel"}"#).is_err());
        assert!(parse_request("not json").is_err());
    }

    #[test]
    fn responses_are_single_lines() {
        let line = Response::Stdout {
            data: "one\ntwo\r\n".to_string(),
        }
        .to_line();
        assert_eq!(line.matches('\n').count(), 1, "only the framing newline");
        assert!(line.ends_with('\n'));
        assert!(line.contains(r"one\ntwo"));
    }

    #[test]
    fn started_is_a_bare_tagged_object() {
        assert_eq!(Response::Started.to_line(), "{\"type\":\"started\"}\n");
    }

    #[test]
    fn completed_uses_the_wire_field_names() {
        let line = Response::Completed {
            exit_code: 3,
            duration: 42,
        }
        .to_line();
        assert!(line.contains(r#""type":"completed""#));
        assert!(line.contains(r#""exitCode":3"#));
        assert!(line.contains(r#""duration":42"#));
    }
}
