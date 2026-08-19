/// A tool that forgot to be async.
#[panday_sdk::tool]
fn lookup(_ctx: &panday_harness::tools::ToolCtx, id: String) -> Result<String, String> {
    Ok(id)
}

fn main() {}
