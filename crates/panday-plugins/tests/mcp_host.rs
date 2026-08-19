//! M16.3 — mounting an MCP server and calling its tools (docs/16 §MCP host).
//!
//! The server is `fixture-mcp-server`: a hand-written JSON-RPC implementation, so
//! this suite tests interop rather than `rmcp` agreeing with itself.

use panday_plugins::mcp::{McpError, MountedServer, StdioServer};

fn spec() -> StdioServer {
    StdioServer::new("github", env!("CARGO_BIN_EXE_fixture-mcp-server"))
}

#[tokio::test(flavor = "multi_thread")]
async fn mounting_a_server_lists_its_tools_under_qualified_ids() {
    let server = MountedServer::mount(&spec()).await.expect("mount");

    let ids: Vec<&str> = server.tools().iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&"mcp:github:list_issues"), "{ids:?}");
    assert!(ids.contains(&"mcp:github:create_issue"), "{ids:?}");
    // docs/16 specifies `mcp:{server}:{tool}` — and `Origin::for_tool` decodes
    // provenance from exactly that shape, so the format is load-bearing.
    assert_eq!(
        panday_types::model::Origin::for_tool("mcp:github:list_issues"),
        panday_types::model::Origin::Mcp {
            server: "github".into(),
            tool: "list_issues".into()
        }
    );
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_goes_out_and_a_result_comes_back() {
    let server = MountedServer::mount(&spec()).await.unwrap();
    let (text, is_error) = server
        .call(
            "mcp:github:list_issues",
            serde_json::json!({"repo": "hexuria/panday"}),
        )
        .await
        .expect("call");
    assert_eq!(text, "2 open issues in hexuria/panday");
    assert!(!is_error);
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_servers_own_error_reaches_us_as_an_error() {
    // `isError` is the server's judgement and we keep it: an MCP tool reporting
    // failure must reach the model as a tool error so it can correct, not as a
    // success whose text happens to describe a failure.
    let server = MountedServer::mount(&spec()).await.unwrap();
    let (text, is_error) = server
        .call("mcp:github:explode", serde_json::json!({}))
        .await
        .unwrap();
    assert!(is_error);
    assert!(text.contains("does not exist"), "{text}");
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn non_text_content_is_reported_rather_than_dropped() {
    // A tool result that silently discarded an image would leave the model unable to
    // tell "nothing was returned" from "something I cannot see was returned".
    let server = MountedServer::mount(&spec()).await.unwrap();
    let (text, _) = server
        .call("mcp:github:picture", serde_json::json!({}))
        .await
        .unwrap();
    assert!(text.contains("image content omitted"), "{text}");
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_inherits_no_environment() {
    // docs/20 T4 and docs/14's `--clearenv`: an MCP server is third-party code, and a
    // token it was not granted must not be visible to it.
    std::env::set_var("PANDAY_MCP_LEAK_CANARY", "should-not-be-visible");
    let server = MountedServer::mount(&spec()).await.unwrap();
    let (text, _) = server
        .call("mcp:github:leak_env", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(text, "env_vars=0", "the server saw our environment: {text}");
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declared_env_var_does_reach_the_server() {
    // The other half: a grant that does not work is not a grant. Without this, the
    // test above would pass with a broken `env()` builder.
    let server = MountedServer::mount(&spec().env("GITHUB_TOKEN", "ghp_x"))
        .await
        .unwrap();
    let (text, _) = server
        .call("mcp:github:leak_env", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(text, "env_vars=1", "{text}");
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_brief_form_is_one_line_and_carries_no_schema() {
    // docs/16 §schema hygiene: a tool schema lands in the stable cached prefix
    // (ADR-008), so a 40kB schema is 40kB paid every turn of the session.
    let server = MountedServer::mount(&spec()).await.unwrap();
    let brief = server
        .tools()
        .iter()
        .find(|t| t.id.ends_with("list_issues"))
        .unwrap()
        .brief();
    assert!(!brief.contains('\n'), "{brief}");
    assert!(brief.contains("List issues in a repository."), "{brief}");
    assert!(
        !brief.contains("second line"),
        "the brief form must drop the rest: {brief}"
    );
    assert!(
        !brief.contains("inputSchema") && !brief.contains("properties"),
        "{brief}"
    );

    let index = server.brief_index();
    assert_eq!(index.lines().count(), server.tools().len());
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_tool_is_refused_locally() {
    // Without a round trip: we know what the server offered at mount time, and
    // asking it about a tool it never listed would just be a slower error.
    let server = MountedServer::mount(&spec()).await.unwrap();
    let err = server
        .call("mcp:github:nope", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::NoSuchTool(_)), "{err:?}");
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn non_object_arguments_are_refused_before_the_wire() {
    let server = MountedServer::mount(&spec()).await.unwrap();
    let err = server
        .call("mcp:github:list_issues", serde_json::json!("just a string"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("must be a JSON object"), "{err}");
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_will_not_start_is_named_in_the_error() {
    let err = MountedServer::mount(&StdioServer::new("ghost", "/nonexistent/mcp-server"))
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::Spawn { .. }), "{err:?}");
    assert!(err.to_string().contains("ghost"), "{err}");
}
