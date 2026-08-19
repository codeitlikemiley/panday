/// A generic tool has no single schema.
#[panday_sdk::tool]
async fn lookup<T: ToString>(
    _ctx: &panday_harness::tools::ToolCtx,
    id: T,
) -> Result<String, String> {
    Ok(id.to_string())
}

fn main() {}
