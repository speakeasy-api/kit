//! `X-Cerebras-Version-Patch` header helper.

/// Name of the version-patch header.
pub const VERSION_PATCH_HEADER: &str = "X-Cerebras-Version-Patch";

/// Formats the numeric patch version as a header value.
pub fn format_version_patch(v: u32) -> String {
    v.to_string()
}
