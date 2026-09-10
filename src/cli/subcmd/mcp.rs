//! `fermut mcp` — Model Context Protocol server over stdio.
//!
//! Lets a coding agent (Claude Code, Cursor, …) drive fermut as a native
//! tool instead of shelling out and parsing CLI output. The transport is
//! the MCP stdio convention: newline-delimited JSON-RPC 2.0 messages on
//! stdin/stdout. One JSON object per line, no embedded newlines, no
//! Content-Length framing.
//!
//! **stdout is the protocol channel** — every byte fermut writes there must
//! be a JSON-RPC message, so tool handlers return structured data that the
//! server wraps; they never `println!`. Logs and progress go to stderr.
//!
//! The loop is synchronous: read a line, dispatch, write the response. No
//! async runtime — requests are handled one at a time, which is all an
//! inner-loop agent needs.
//!
//! Tools (each mirrors the same-named subcommand and emits the same JSON):
//! - `fermut_run` — run mutation testing, return the summary + report path.
//! - `fermut_next` — rank survivors in a report by what to fix next.
//! - `fermut_explain` — why one mutant survived + a killing-test skeleton.
//! - `fermut_score` — the reward signal (score delta vs a baseline run).
//! - `fermut_list_survivors` — survivors + timeouts from a report.
//! - `fermut_doctor` — environment/config preflight before a run.
//! - `fermut_baseline` — day-one coverage-vs-mutation baseline + grade.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::report::Report;

/// MCP protocol version this server defaults to when the client doesn't
/// announce one. When the client does, we echo theirs back — the spec lets
/// server and client negotiate, and echoing the client's is the most
/// compatible choice.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Deserialize)]
struct Request {
    /// Present on requests, absent on notifications. Drives whether we reply.
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// A JSON-RPC error payload. Codes follow the JSON-RPC 2.0 spec (-32xxx).
#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
        }
    }
    fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
        }
    }
}

/// Run the server loop until stdin closes (EOF / client disconnect).
pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                // Parse error — no id to attach, send a null-id error per spec.
                write_message(
                    &mut out,
                    &error_response(
                        Value::Null,
                        RpcError {
                            code: -32700,
                            message: format!("parse error: {e}"),
                        },
                    ),
                )?;
                continue;
            }
        };
        // Notifications (no id) are fire-and-forget: handle side effects,
        // never reply. `notifications/initialized` is the only one we expect.
        let Some(id) = req.id.clone() else {
            continue;
        };
        let response = match dispatch(&req.method, &req.params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => error_response(id, e),
        };
        write_message(&mut out, &response)?;
    }
    Ok(())
}

fn write_message(out: &mut impl Write, msg: &Value) -> Result<()> {
    writeln!(out, "{}", serde_json::to_string(msg)?)?;
    out.flush()?;
    Ok(())
}

fn error_response(id: Value, e: RpcError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": e.code, "message": e.message}})
}

/// Route a request method to its handler. Returns the JSON-RPC `result`
/// payload, or an `RpcError` for protocol-level failures. Tool *execution*
/// failures are not protocol errors — they come back inside a successful
/// `tools/call` result with `isError: true`, so the agent sees the message.
fn dispatch(method: &str, params: &Value) -> Result<Value, RpcError> {
    match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_definitions()})),
        "tools/call" => tools_call(params),
        other => Err(RpcError::method_not_found(other)),
    }
}

fn initialize(params: &Value) -> Value {
    // Echo the client's protocol version when offered; else our default.
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "fermut", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// The advertised tool catalogue. Each entry is `{name, description,
/// inputSchema}` where `inputSchema` is a JSON Schema object describing the
/// `arguments` the agent passes to `tools/call`.
fn tool_definitions() -> Value {
    json!([
        {
            "name": "fermut_run",
            "description": "Run mutation testing on a Python project and return the score summary. Writes a JSON report (consumed by fermut_next / fermut_list_survivors) and appends to the run history (consumed by fermut_score).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Source root to mutate. Defaults to the configured source_root or '.'."},
                    "tests": {"type": "string", "description": "Test directory passed to the runner. Defaults to the configured tests dir."},
                    "coverage": {"type": "string", "description": "Path to a coverage file for per-mutant test selection."},
                    "since": {"type": "string", "description": "Restrict mutants to lines changed since this git ref/date (includes uncommitted edits)."},
                    "diff_only": {"type": "string", "description": "Restrict mutants to lines changed vs this base ref."},
                    "jobs": {"type": "integer", "description": "Parallel worker count. Defaults to logical CPU count."},
                    "timeout": {"type": "integer", "description": "Per-mutant pytest timeout in seconds."},
                    "max_time": {"type": "integer", "description": "Wall-clock ceiling (seconds) on the testing phase. Highest-value mutants (covered first) run before the deadline; the rest are recorded skipped/`time-budget` and excluded from the score. Bounds testing only, not baseline/generation/ty. Use for a predictable inner-loop latency cap on large suites."},
                    "python": {"type": "string", "description": "Python interpreter path or virtualenv dir to run pytest with (`<python> -m pytest`); avoids relying on PATH. Auto-discovers a venv when omitted."},
                    "report_path": {"type": "string", "description": "Where to write the JSON report. Defaults to <project>/.fermut/last.json."}
                }
            }
        },
        {
            "name": "fermut_next",
            "description": "Rank surviving mutants in a JSON report by which one to fix next: cluster leverage (one test often kills a whole file+operator pattern) then kill-ease, with an estimated score gain per cluster.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "report": {"type": "string", "description": "Path to a JSON report from fermut_run / `fermut run --json`."},
                    "limit": {"type": "integer", "description": "Number of ranked survivors to return. Defaults to all."},
                    "max_tokens": {"type": "integer", "description": "Cap the result at this estimated token budget; keeps the highest-ranked that fit."}
                },
                "required": ["report"]
            }
        },
        {
            "name": "fermut_explain",
            "description": "Explain why one surviving mutant likely wasn't caught and propose a killing test: operator-specific hint, enclosing symbol, source context, optional coverage signal, and a pytest skeleton. Pure heuristic — no network, no LLM cost.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "report": {"type": "string", "description": "Path to a JSON report from fermut_run / `fermut run --json`."},
                    "target": {"type": "string", "description": "Mutant selector: 1-based index into the report, or a substring of the mutant id (e.g. from fermut_next)."},
                    "context": {"type": "integer", "description": "Source lines of context on each side of the mutant line. Default 5."},
                    "tests": {"type": "string", "description": "Tests directory to grep for the enclosing symbol."},
                    "coverage": {"type": "string", "description": "Path to a coverage file; adds whether any test executed the mutant line."}
                },
                "required": ["report", "target"]
            }
        },
        {
            "name": "fermut_score",
            "description": "Emit the agent reward signal for the latest run: mutation score, delta vs a baseline run, and the new-survivor / newly-killed mutant-id diff. Reads .fermut/history.jsonl.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Project root to resolve history from. Defaults to '.'."},
                    "baseline": {"type": "integer", "description": "Compare against the entry this many branch-comparable runs back. Default 1."},
                    "branch": {"type": "string", "description": "Restrict current/baseline selection to this git branch."}
                }
            }
        },
        {
            "name": "fermut_list_survivors",
            "description": "List surviving and timed-out mutants from a JSON report — id, file, line, operator, and the original→replacement edit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "report": {"type": "string", "description": "Path to a JSON report from fermut_run / `fermut run --json`."}
                },
                "required": ["report"]
            }
        },
        {
            "name": "fermut_doctor",
            "description": "Preflight the environment and config: which tools are present (pytest, coverage, pytest-cov, ty), whether coverage has per-test contexts, and common gotchas. Returns a checks list plus a `healthy` flag. Run before fermut_run to catch setup problems early.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Project root to diagnose. Defaults to '.'."}
                }
            }
        },
        {
            "name": "fermut_baseline",
            "description": "Day-one baseline for a project: builds coverage, runs a fast sampled mutation pass over covered code, and returns line coverage, mutation score, the test-quality gap between them, a grade band, and the worst files by survivor count. Run this first to see where a suite stands; then fermut_next to act on it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Project root. Defaults to '.'."},
                    "full": {"type": "boolean", "description": "Mutate every covered mutant for the exact score instead of a sampled estimate. Slower."},
                    "sample": {"type": "number", "description": "Sampling fraction (0.0–1.0) for the fast pass. Defaults to 0.1. Ignored when full=true."},
                    "top": {"type": "integer", "description": "How many worst-offender files to list. Defaults to 3."}
                }
            }
        }
    ])
}

/// Handle `tools/call`: validate the envelope, dispatch by tool name, and
/// wrap the outcome. A handler that returns `Ok` becomes a normal text
/// result; one that returns `Err` becomes an `isError: true` result so the
/// agent reads the failure as data rather than a transport fault.
fn tools_call(params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("tools/call requires a string `name`"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let outcome = match name {
        "fermut_run" => tool_run(&args),
        "fermut_next" => tool_next(&args),
        "fermut_explain" => tool_explain(&args),
        "fermut_score" => tool_score(&args),
        "fermut_list_survivors" => tool_list_survivors(&args),
        "fermut_doctor" => tool_doctor(&args),
        "fermut_baseline" => tool_baseline(&args),
        other => return Err(RpcError::invalid_params(format!("unknown tool: {other}"))),
    };

    Ok(match outcome {
        Ok(value) => tool_result(&value, false),
        Err(e) => tool_result(&Value::String(e.to_string()), true),
    })
}

/// Wrap a payload in the MCP `tools/call` result envelope. Structured data
/// is serialized to a JSON string in a single text content block — the
/// interoperable shape every MCP client renders.
fn tool_result(value: &Value, is_error: bool) -> Value {
    let text = match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

// --- Tool implementations. Each returns the structured payload or an error
// message; the envelope wrapping happens in `tools_call`. ---

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn load_report(path: &str) -> Result<Report> {
    // Prevent path traversal attacks by rejecting paths containing '..'.
    let path_buf = std::path::Path::new(path);
    if path_buf
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(anyhow::anyhow!("Invalid input: {}", path_buf.display()));
    }
    crate::report::load(path_buf)
}

fn tool_next(args: &Value) -> Result<Value> {
    let report_path =
        str_arg(args, "report").ok_or_else(|| anyhow::anyhow!("`report` is required"))?;
    let report = load_report(report_path)?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let max_tokens = args
        .get("max_tokens")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let (shown, total) = super::next::rank_report(&report, limit, max_tokens);
    Ok(json!({
        "survivors": shown,
        "shown": shown.len(),
        "total": total,
        "omitted": total - shown.len(),
    }))
}

fn tool_explain(args: &Value) -> Result<Value> {
    use super::explain::{build_explain_report, ExplainOpts};
    use crate::cli::Format;
    let report = str_arg(args, "report").ok_or_else(|| anyhow::anyhow!("`report` is required"))?;
    let target = str_arg(args, "target").ok_or_else(|| anyhow::anyhow!("`target` is required"))?;
    let context_lines = args
        .get("context")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(5);
    // Prevent path traversal attacks by rejecting paths containing '..'.
    let report_path = std::path::Path::new(report);
    if report_path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(anyhow::anyhow!("Invalid input: {}", report_path.display()));
    }
    let opts = ExplainOpts {
        report: PathBuf::from(report),
        target: target.to_string(),
        context_lines,
        tests: str_arg(args, "tests").map(PathBuf::from),
        coverage: str_arg(args, "coverage").map(PathBuf::from),
        // The agent is itself an LLM with the repo loaded — give it the
        // heuristic signal and let it write the test. No second model call.
        llm: false,
        model: None,
        no_cache: false,
        cache_path: None,
        project_root: crate::cli::current_project_root(),
        format: Format::Json,
    };
    Ok(serde_json::to_value(build_explain_report(&opts)?)?)
}

fn tool_doctor(args: &Value) -> Result<Value> {
    let path = PathBuf::from(str_arg(args, "path").unwrap_or("."));
    Ok(super::doctor::diagnose(&path))
}

fn tool_baseline(args: &Value) -> Result<Value> {
    use super::baseline::{compute_baseline, BaselineArgs};
    use crate::cli::Format;
    let path = PathBuf::from(str_arg(args, "path").unwrap_or("."));
    let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
    let sample = args.get("sample").and_then(Value::as_f64);
    let top = args
        .get("top")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(3);
    let opts = BaselineArgs {
        path,
        full,
        sample,
        top,
        format: Format::Json,
        // No filter overrides over MCP — compute_baseline forces the
        // coverage filter on so the score is over covered code.
        filter: super::super::FilterArgs {
            ops: None,
            skip_ops: None,
            diff_only: None,
            since: None,
            no_diff_only: false,
            coverage: None,
            no_coverage: false,
            experimental: false,
            parity: false,
            exclude: Vec::new(),
        },
    };
    Ok(serde_json::to_value(compute_baseline(opts)?)?)
}

fn tool_score(args: &Value) -> Result<Value> {
    let path = PathBuf::from(str_arg(args, "path").unwrap_or("."));
    let baseline = args
        .get("baseline")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(1);
    let branch = str_arg(args, "branch");
    let history_path = crate::history::default_history_path(&crate::history::resolve_root(&path));
    match super::score::compute_score(&history_path, baseline, branch)? {
        Some(report) => Ok(serde_json::to_value(report)?),
        None => Err(anyhow::anyhow!(
            "no history at {} — run fermut_run once first",
            history_path.display()
        )),
    }
}

fn tool_list_survivors(args: &Value) -> Result<Value> {
    use crate::report::MutantOutcome;
    let report_path =
        str_arg(args, "report").ok_or_else(|| anyhow::anyhow!("`report` is required"))?;
    let report = load_report(report_path)?;
    let survivors: Vec<Value> = report
        .outcomes
        .iter()
        .filter(|o| {
            matches!(
                o,
                MutantOutcome::Survived { .. } | MutantOutcome::TimedOut { .. }
            )
        })
        .map(|o| {
            let m = o.mutant();
            json!({
                "status": o.status_label(),
                "id": m.id,
                "file": m.file.display().to_string(),
                "line": m.line,
                "operator": m.operator.name(),
                "original": m.original,
                "replacement": m.replacement,
            })
        })
        .collect();
    Ok(json!({"survivors": survivors, "count": survivors.len()}))
}

fn tool_run(args: &Value) -> Result<Value> {
    // Prevent path traversal attacks by rejecting paths containing '..'.
    let path_str = str_arg(args, "path").unwrap_or(".");
    let path = std::path::Path::new(path_str);
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(anyhow::anyhow!("Invalid input: {}", path.display()));
    }
    let path = PathBuf::from(path_str);
    let tests = str_arg(args, "tests").map(PathBuf::from);
    let coverage = str_arg(args, "coverage").map(PathBuf::from);
    let since = str_arg(args, "since").map(str::to_string);
    let diff_only = str_arg(args, "diff_only").map(str::to_string);
    let jobs = args.get("jobs").and_then(Value::as_u64).map(|n| n as usize);
    let timeout = args.get("timeout").and_then(Value::as_u64);
    let max_time = args.get("max_time").and_then(Value::as_u64);
    let python = str_arg(args, "python").map(PathBuf::from);

    let filter = super::super::FilterArgs {
        ops: None,
        skip_ops: None,
        diff_only,
        since,
        no_diff_only: false,
        coverage,
        no_coverage: false,
        experimental: false,
        parity: false,
        exclude: Vec::new(),
    };
    // Build the runtime config the same way `fermut run` does, so a project's
    // fermut.toml is honored and only the explicitly-passed args override it.
    let cfg = crate::cli::build_config::build_config(
        path,
        crate::cli::RunConfigArgs {
            tests,
            jobs,
            timeout,
            python,
            max_time,
            filter,
            ..Default::default()
        },
    )?;

    let (report, _entry) = crate::engine::run(&cfg)?;

    // Persist the report so fermut_next / fermut_list_survivors can read it.
    let report_path = str_arg(args, "report_path")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            crate::history::resolve_root(&cfg.source_root)
                .join(".fermut")
                .join("last.json")
        });
    report.write_json(&report_path)?;

    let summary = serde_json::to_value(report.summary())?;
    Ok(json!({
        "summary": summary,
        "report_path": report_path.display().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_echoes_client_protocol_version() {
        let params = json!({"protocolVersion": "2024-11-05"});
        let r = initialize(&params);
        assert_eq!(r["protocolVersion"], "2024-11-05");
        assert_eq!(r["serverInfo"]["name"], "fermut");
        assert!(r["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_falls_back_to_default_version() {
        let r = initialize(&json!({}));
        assert_eq!(r["protocolVersion"], DEFAULT_PROTOCOL_VERSION);
    }

    #[test]
    fn tools_list_advertises_every_tool() {
        let tools = tool_definitions();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for expected in [
            "fermut_run",
            "fermut_next",
            "fermut_explain",
            "fermut_score",
            "fermut_list_survivors",
            "fermut_doctor",
            "fermut_baseline",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
    }

    #[test]
    fn fermut_run_advertises_max_time() {
        // The budget flag must be reachable over MCP, not just the CLI.
        let run = tool_definitions()
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "fermut_run")
            .unwrap()
            .clone();
        assert_eq!(
            run["inputSchema"]["properties"]["max_time"]["type"], "integer",
            "fermut_run must expose an integer max_time"
        );
    }

    #[test]
    fn every_tool_has_object_input_schema() {
        for t in tool_definitions().as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object", "tool {}", t["name"]);
        }
    }

    #[test]
    fn dispatch_unknown_method_is_method_not_found() {
        let err = dispatch("bogus/method", &json!({})).unwrap_err();
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn dispatch_ping_is_empty_object() {
        assert_eq!(dispatch("ping", &json!({})).unwrap(), json!({}));
    }

    #[test]
    fn tools_call_requires_name() {
        let err = tools_call(&json!({"arguments": {}})).unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn tools_call_unknown_tool_is_invalid_params() {
        let err = tools_call(&json!({"name": "fermut_nope", "arguments": {}})).unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn next_missing_report_is_tool_error_not_protocol_error() {
        // A missing required arg surfaces as a tools/call result with
        // isError:true, not a JSON-RPC error — the agent reads it as data.
        let result = tools_call(&json!({"name": "fermut_next", "arguments": {}})).unwrap();
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("report"), "got: {text}");
    }

    #[test]
    fn next_ranks_survivors_from_a_report_file() {
        let tmp = tempfile::tempdir().unwrap();
        let report = tmp.path().join("r.json");
        std::fs::write(
            &report,
            r#"{"outcomes":[
                {"status":"killed","mutant":{"id":"a.py@1:arith-op-swap:+->-","file":"a.py","operator":"arith-op-swap","range":[1,2],"original":"+","replacement":"-","line":1}},
                {"status":"survived","mutant":{"id":"a.py@40:boundary-shift:>=->>","file":"a.py","operator":"boundary-shift","range":[40,42],"original":">=","replacement":">","line":12}},
                {"status":"survived","mutant":{"id":"a.py@60:boundary-shift:<=-><","file":"a.py","operator":"boundary-shift","range":[60,62],"original":"<=","replacement":"<","line":13}}
            ]}"#,
        )
        .unwrap();
        let result = tools_call(
            &json!({"name": "fermut_next", "arguments": {"report": report.display().to_string()}}),
        )
        .unwrap();
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["total"], 1); // two boundary survivors → one cluster
        assert_eq!(payload["survivors"][0]["cluster_size"], 2);
    }

    #[test]
    fn list_survivors_returns_survivors_and_timeouts() {
        let tmp = tempfile::tempdir().unwrap();
        let report = tmp.path().join("r.json");
        std::fs::write(
            &report,
            r#"{"outcomes":[
                {"status":"survived","mutant":{"id":"a.py@1:boundary-shift:>=->>","file":"a.py","operator":"boundary-shift","range":[1,3],"original":">=","replacement":">","line":1}},
                {"status":"killed","mutant":{"id":"a.py@9:arith-op-swap:+->-","file":"a.py","operator":"arith-op-swap","range":[9,10],"original":"+","replacement":"-","line":2}}
            ]}"#,
        )
        .unwrap();
        let result = tools_call(
            &json!({"name": "fermut_list_survivors", "arguments": {"report": report.display().to_string()}}),
        )
        .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["count"], 1);
        assert_eq!(payload["survivors"][0]["operator"], "boundary-shift");
    }

    #[test]
    fn explain_returns_hint_and_skeleton_for_a_survivor() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.py");
        std::fs::write(&src, "def f(x):\n    return x >= 18\n").unwrap();
        let report = tmp.path().join("r.json");
        let id = format!("{}@20:boundary-shift:>=->>", src.display());
        // Build via serde_json so Windows paths get their backslashes escaped.
        let report_json = json!({"outcomes":[{"status":"survived","mutant":{
            "id": id,
            "file": src.display().to_string(),
            "operator": "boundary-shift",
            "range": [20, 22],
            "original": ">=",
            "replacement": ">",
            "line": 2
        }}]});
        std::fs::write(&report, serde_json::to_string(&report_json).unwrap()).unwrap();
        let result = tools_call(&json!({
            "name": "fermut_explain",
            "arguments": {"report": report.display().to_string(), "target": "1"}
        }))
        .unwrap();
        assert_eq!(result["isError"], false);
        let payload: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["mutant"]["operator"], "boundary-shift");
        assert!(payload["hint"].as_str().unwrap().contains("boundary"));
        assert!(payload["skeleton"].is_string() || payload["skeleton"].is_object());
    }

    #[test]
    fn explain_missing_target_is_tool_error() {
        let result = tools_call(&json!({
            "name": "fermut_explain", "arguments": {"report": "/nope.json"}
        }))
        .unwrap();
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("target"));
    }

    #[test]
    fn doctor_returns_checks_and_healthy_flag() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        let result = tools_call(&json!({
            "name": "fermut_doctor", "arguments": {"path": tmp.path().display().to_string()}
        }))
        .unwrap();
        assert_eq!(result["isError"], false);
        let payload: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(payload["checks"].is_array());
        assert!(payload["summary"]["fail"].is_number());
        assert!(payload["healthy"].is_boolean());
    }

    #[test]
    fn baseline_in_broken_env_is_tool_error_not_protocol_error() {
        // No python/pytest/coverage in a bare temp dir → doctor reports the
        // env unhealthy → baseline refuses, surfacing as isError data rather
        // than a JSON-RPC transport fault.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        let result = tools_call(&json!({
            "name": "fermut_baseline", "arguments": {"path": tmp.path().display().to_string()}
        }))
        .unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn score_with_no_history_is_tool_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        let result = tools_call(
            &json!({"name": "fermut_score", "arguments": {"path": tmp.path().display().to_string()}}),
        )
        .unwrap();
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("no history"), "got: {text}");
    }
}
