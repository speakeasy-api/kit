use std::error::Error;

use mcp_reference_interop::{ReferenceImplementation, probe_reference_implementation};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let implementation = std::env::args()
        .nth(1)
        .as_deref()
        .and_then(ReferenceImplementation::parse)
        .unwrap_or(ReferenceImplementation::RustSdkStatefulSse);

    let result = probe_reference_implementation(implementation).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
