/// A tool with no way to report failure.
#[panday_sdk::tool]
async fn lookup(_ctx: &panday_harness::tools::ToolCtx, id: String) {
    let _ = id;
}

fn main() {}
