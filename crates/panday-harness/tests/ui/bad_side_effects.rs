/// A tool declaring a side effect nobody defined.
#[panday_sdk::tool(side_effects = "mostly harmless")]
async fn lookup(_ctx: &panday_harness::tools::ToolCtx, id: String) -> Result<String, String> {
    Ok(id)
}

fn main() {}
