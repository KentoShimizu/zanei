use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use zanei_core::schema::KNOWN_EVENT_TYPES;
use zanei_core::timeline::MIN_TIMELINE_TOKEN_BUDGET_TOKENS;

mod support;

use support::{Fixture, damaged_set_aside_store};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const EVENT_SCHEMA: &str = include_str!("../../../docs/public/schema/event.schema.json");

#[test]
fn mcp_stdio_exposes_three_tools_and_contract_results() {
    let fixture = Fixture::populated();
    let mut client = McpClient::start(&fixture);

    initialize(&mut client);
    let listed = client.request("tools/list", json!({}));
    let tools = listed["result"]["tools"]
        .as_array()
        .expect("tools/list result");
    let names: BTreeSet<_> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name must be a string"))
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["get_status", "get_timeline", "query_events"])
    );
    assert!(
        tools
            .iter()
            .all(|tool| tool["annotations"]["readOnlyHint"] == true)
    );
    let timeline_tool = tools
        .iter()
        .find(|tool| tool["name"] == "get_timeline")
        .expect("get_timeline tool schema");
    assert_eq!(
        timeline_tool["inputSchema"]["properties"]["token_budget"]["minimum"],
        MIN_TIMELINE_TOKEN_BUDGET_TOKENS
    );

    let status = client.call_tool("get_status", json!({}));
    assert_keys(
        &status,
        &[
            "capture",
            "collector_failures",
            "degraded",
            "events_dropped",
            "last_event_ts",
            "oldest_event_ts",
            "paused",
            "permissions_ok",
            "retention_hours",
            "running",
        ],
    );
    assert_eq!(status["running"], true);
    assert_eq!(status["paused"], false);
    assert_eq!(status["retention_hours"], 48);
    assert_eq!(status["capture"]["sources"], json!(["app"]));
    assert_eq!(status["capture"]["text_content"], false);
    assert_eq!(status["permissions_ok"], true);
    assert_eq!(status["events_dropped"], 2);
    assert_eq!(status["degraded"], json!({}));
    assert_eq!(status["collector_failures"]["eventtap"], 1);
    assert!(status["last_event_ts"].is_string());
    assert!(status["oldest_event_ts"].is_string());

    let query = client.call_tool("query_events", json!({}));
    assert_keys(&query, &["count", "events", "range", "truncated"]);
    assert_eq!(query["count"], KNOWN_EVENT_TYPES.len());
    assert_eq!(query["truncated"], false);
    let events = query["events"].as_array().expect("query events array");
    assert_eq!(events.len(), KNOWN_EVENT_TYPES.len());
    assert_keys(
        &events[0],
        &[
            "app",
            "data",
            "element",
            "id",
            "mono_ns",
            "redaction",
            "source",
            "truncated",
            "ts",
            "type",
            "v",
            "window",
        ],
    );
    assert_eq!(events[0]["truncated"], false);
    let event_types: BTreeSet<_> = events
        .iter()
        .map(|event| event["type"].as_str().expect("event type"))
        .collect();
    assert_eq!(
        event_types,
        KNOWN_EVENT_TYPES.iter().copied().collect::<BTreeSet<_>>()
    );

    let timeline = client.call_tool("get_timeline", json!({}));
    assert_keys(
        &timeline,
        &["content", "format", "range", "token_estimate", "truncated"],
    );
    assert_eq!(timeline["format"], "markdown");
    assert!(timeline["range"]["since"].is_string());
    assert!(timeline["range"]["until"].is_string());
    assert!(timeline["content"].as_str().is_some_and(|content| {
        content.contains("Zanei timeline") && content.contains("FixtureApp")
    }));
    assert!(timeline["token_estimate"].is_number());
    assert_eq!(timeline["truncated"], false);
}

#[test]
fn tool_results_validate_against_the_listed_output_schemas() {
    let fixture = Fixture::populated();
    let mut client = McpClient::start(&fixture);
    initialize(&mut client);
    let listed = client.request("tools/list", json!({}));
    let schemas: BTreeMap<_, _> = listed["result"]["tools"]
        .as_array()
        .expect("tools/list result")
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().expect("tool name"),
                &tool["outputSchema"],
            )
        })
        .collect();
    let canonical_event_schema: Value =
        serde_json::from_str(EVENT_SCHEMA).expect("canonical event schema JSON");
    assert_eq!(
        schemas["query_events"].pointer("/properties/events/items"),
        Some(&canonical_event_schema),
        "query_events events.items must embed the canonical event schema unchanged"
    );

    let status = client.call_tool("get_status", json!({}));
    let query = client.call_tool("query_events", json!({ "limit": 2 }));
    let markdown = client.call_tool("get_timeline", json!({ "format": "markdown" }));
    let structured = client.call_tool(
        "get_timeline",
        json!({ "format": "structured", "granularity": "fine" }),
    );
    assert_eq!(structured["sessions"][0]["event_ids_truncated"], false);

    for (name, output) in [
        ("get_status", &status),
        ("query_events", &query),
        ("get_timeline", &markdown),
        ("get_timeline", &structured),
    ] {
        let schema = schemas.get(name).expect("listed output schema");
        let validator = jsonschema::draft202012::options()
            .build(schema)
            .unwrap_or_else(|error| panic!("invalid {name} output schema: {error}"));
        assert!(
            validator.is_valid(output),
            "{name} output failed its listed schema: {:?}",
            validator.iter_errors(output).collect::<Vec<_>>()
        );
    }

    let timeline_validator = jsonschema::draft202012::options()
        .build(schemas["get_timeline"])
        .expect("valid get_timeline output schema");
    let mut markdown_with_sessions = markdown.clone();
    markdown_with_sessions
        .as_object_mut()
        .expect("markdown output object")
        .insert("sessions".to_owned(), json!([]));
    let mut structured_with_content = structured.clone();
    structured_with_content
        .as_object_mut()
        .expect("structured output object")
        .insert("content".to_owned(), json!("invalid"));
    let mut markdown_without_content = markdown.clone();
    markdown_without_content
        .as_object_mut()
        .expect("markdown output object")
        .remove("content");
    let mut markdown_with_unknown_field = markdown.clone();
    markdown_with_unknown_field
        .as_object_mut()
        .expect("markdown output object")
        .insert("unknown".to_owned(), json!(true));
    let mut markdown_with_structured_format = markdown.clone();
    markdown_with_structured_format["format"] = json!("structured");

    for invalid in [
        markdown_with_sessions,
        structured_with_content,
        markdown_without_content,
        markdown_with_unknown_field,
        markdown_with_structured_format,
    ] {
        assert!(
            !timeline_validator.is_valid(&invalid),
            "get_timeline schema accepted invalid output: {invalid}"
        );
    }
}

#[test]
fn initialize_reports_the_cli_package_version() {
    let fixture = Fixture::empty();
    let mut client = McpClient::start(&fixture);
    let response = initialize(&mut client);
    let version_output = fixture
        .command()
        .arg("--version")
        .output()
        .expect("zanei --version output");
    assert!(version_output.status.success());
    let cli_version = String::from_utf8(version_output.stdout).expect("UTF-8 version output");
    let cli_version = cli_version
        .split_whitespace()
        .nth(1)
        .expect("zanei version number");

    assert_eq!(response["result"]["serverInfo"]["version"], cli_version);
}

#[test]
fn query_events_reports_truncation_and_invalid_params() {
    let fixture = Fixture::populated();
    let mut client = McpClient::start(&fixture);
    initialize(&mut client);

    let limited = client.call_tool("query_events", json!({ "limit": 2 }));
    assert_eq!(limited["count"], 2);
    assert_eq!(limited["events"].as_array().map(Vec::len), Some(2));
    assert_eq!(limited["truncated"], true);

    assert_invalid_params(client.call_tool_response("query_events", json!({ "since": "bogus" })));
    assert_invalid_params(client.call_tool_response("query_events", json!({ "limit": 1_001 })));
}

#[test]
fn get_timeline_rejects_token_budget_below_minimum_as_invalid_params() {
    let fixture = Fixture::uninitialized();
    let mut client = McpClient::start(&fixture);
    initialize(&mut client);

    let response = client.call_tool_response(
        "get_timeline",
        json!({ "token_budget": MIN_TIMELINE_TOKEN_BUDGET_TOKENS - 1 }),
    );

    assert_eq!(
        response["error"]["message"],
        format!("token_budget must be at least {MIN_TIMELINE_TOKEN_BUDGET_TOKENS}")
    );
    assert_invalid_params(response);
    assert!(!fixture.store.exists());
}

#[test]
fn get_status_prefers_the_running_recorder_permission_snapshot() {
    let fixture = Fixture::populated();
    fixture.set_recorder_permissions(false);
    let mut client = McpClient::start(&fixture);
    initialize(&mut client);

    let status = client.call_tool("get_status", json!({}));

    assert_eq!(status["running"], true);
    assert_eq!(status["permissions_ok"], false);
}

#[test]
fn get_status_prefers_the_running_recorder_retention_snapshot() {
    let fixture = Fixture::populated();
    std::fs::write(
        &fixture.config,
        "[capture]\nsources = [\"app\"]\n[output]\nretention_hours = 72\n",
    )
    .expect("updated config retention");
    let mut client = McpClient::start(&fixture);
    initialize(&mut client);

    let status = client.call_tool("get_status", json!({}));

    assert_eq!(status["retention_hours"], 48);
}

#[test]
fn get_status_reports_a_set_aside_store_the_reader_could_not_attach() {
    let fixture = Fixture::populated();
    let retired = damaged_set_aside_store(&fixture.store, 1);
    let mut client = McpClient::start(&fixture);
    initialize(&mut client);

    let status = client.call_tool("get_status", json!({}));

    let reported = status["degraded"]["retired_store"]
        .as_str()
        .unwrap_or_default();
    assert!(
        reported.contains(retired.file_name().unwrap().to_str().unwrap()),
        "status names the skipped file: {status}"
    );
    let query = client.call_tool("query_events", json!({}));
    assert!(
        query["count"].as_u64().unwrap_or(0) > 0,
        "the live store is still read: {query}"
    );
}

#[test]
fn initialized_empty_store_returns_empty_results() {
    let fixture = Fixture::empty();
    assert!(fixture.store.exists());
    let mut client = McpClient::start(&fixture);
    initialize(&mut client);

    assert_empty_results(&mut client);
}

#[test]
fn uninitialized_store_returns_empty_results_without_creating_a_database() {
    let fixture = Fixture::uninitialized();
    assert!(!fixture.store.exists());
    {
        let mut client = McpClient::start(&fixture);
        initialize(&mut client);
        assert_empty_results(&mut client);
        assert_invalid_params(
            client.call_tool_response("query_events", json!({ "types": ["browser*invalid"] })),
        );
    }
    assert!(!fixture.store.exists());
}

fn initialize(client: &mut McpClient) -> Value {
    let response = client.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {
                "name": "zanei-e2e",
                "version": "0.1.0"
            }
        }),
    );
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response["result"]["protocolVersion"].is_string());
    assert_eq!(response["result"]["serverInfo"]["name"], "zanei");
    client.notify("notifications/initialized", json!({}));
    response
}

fn assert_empty_results(client: &mut McpClient) {
    let status = client.call_tool("get_status", json!({}));
    assert_eq!(status["running"], false);
    assert_eq!(status["paused"], false);
    assert!(status["last_event_ts"].is_null());
    assert!(status["oldest_event_ts"].is_null());
    assert_eq!(status["permissions_ok"], true);
    assert_eq!(status["events_dropped"], 0);
    assert_eq!(status["degraded"], json!({}));
    assert_eq!(status["collector_failures"], json!({}));

    let query = client.call_tool("query_events", json!({}));
    assert_eq!(query["count"], 0);
    assert_eq!(query["truncated"], false);
    assert_eq!(query["events"], json!([]));

    let timeline = client.call_tool("get_timeline", json!({ "format": "structured" }));
    assert_eq!(timeline["truncated"], false);
    assert_eq!(timeline["sessions"], json!([]));
}

fn assert_invalid_params(response: Value) {
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["error"]["code"], -32_602);
}

fn assert_keys(value: &Value, expected: &[&str]) {
    let mut actual: Vec<_> = value
        .as_object()
        .expect("JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    actual.sort_unstable();
    assert_eq!(actual, expected);
}

struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: Receiver<Result<Value, String>>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
}

impl McpClient {
    fn start(fixture: &Fixture) -> Self {
        let mut child = fixture
            .process_command()
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start zanei mcp");
        let stdin = child.stdin.take().expect("MCP stdin");
        let stdout = child.stdout.take().expect("MCP stdout");
        let (sender, responses) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let parsed = line
                    .map_err(|error| format!("failed to read MCP response: {error}"))
                    .and_then(|line| {
                        serde_json::from_str(&line)
                            .map_err(|error| format!("invalid MCP response {line:?}: {error}"))
                    });
                if sender.send(parsed).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            responses,
            reader: Some(reader),
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }));
        let response = self
            .responses
            .recv_timeout(RESPONSE_TIMEOUT)
            .unwrap_or_else(|error| panic!("timed out waiting for {method} response: {error}"))
            .unwrap_or_else(|error| panic!("failed to receive {method} response: {error}"));
        assert_eq!(response["id"], id, "response ID for {method}");
        response
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }));
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.call_tool_response(name, arguments);
        assert!(response.get("error").is_none(), "{name} failed: {response}");
        let structured = &response["result"]["structuredContent"];
        assert!(
            structured.is_object(),
            "{name} did not return result.structuredContent: {response}"
        );
        structured.clone()
    }

    fn call_tool_response(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
        )
    }

    fn write(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("MCP stdin remains open");
        serde_json::to_writer(&mut *stdin, message).expect("write MCP request");
        stdin.write_all(b"\n").expect("terminate MCP request");
        stdin.flush().expect("flush MCP request");
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(PROCESS_POLL_INTERVAL),
                Ok(None) | Err(_) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
