struct Orders;

impl Orders {
    /// A tool is a free function, not a method.
    #[panday_sdk::tool]
    async fn lookup(&self, id: String) -> Result<String, String> {
        Ok(id)
    }
}

fn main() {}
