/// A tool that takes no context, so its calls cannot be attributed.
#[panday_sdk::tool]
async fn lookup() -> Result<String, String> {
    Ok("x".into())
}

fn main() {}
