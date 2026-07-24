//! Runtime build identity shared by the proxy CLI and control-plane seam.

/// BUILD_VERSION is stamped from the release tag by the release workflow.
/// Unstamped source builds use the same identity as the Go control plane.
pub const BUILD_VERSION: &str = match option_env!("CCS_BUILD_VERSION") {
    Some(version) => version,
    None => "dev",
};
