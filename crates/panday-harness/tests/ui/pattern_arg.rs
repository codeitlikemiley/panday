/// A tool whose parameter has no name for the model to fill in.
#[panday_sdk::tool]
async fn lookup(
    _ctx: &panday_harness::tools::ToolCtx,
    (a, b): (String, String),
) -> Result<String, String> {
    Ok(format!("{a}{b}"))
}

fn main() {}
