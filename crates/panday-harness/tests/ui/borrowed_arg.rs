/// A tool whose argument cannot be deserialized from JSON.
#[panday_sdk::tool]
async fn lookup(_ctx: &panday_harness::tools::ToolCtx, id: &str) -> Result<String, String> {
    Ok(id.to_string())
}

fn main() {}
