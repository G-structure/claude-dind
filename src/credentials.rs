use anyhow::{bail, Context, Result};
use std::env;
use std::process::Command;

/// Extracts Claude Code OAuth credentials from the macOS Keychain.
///
/// Claude Code stores its OAuth tokens as a "generic password" in the macOS Keychain
/// with service name `"Claude Code-credentials"` and the current OS username as the
/// account. The credential is a JSON blob containing access tokens, refresh tokens,
/// subscription type, and OAuth scopes.
///
/// We shell out to the `security` CLI rather than using the `security-framework`
/// Rust crate because:
/// - The `security-framework` crate requires the binary to be code-signed with
///   Keychain entitlements. Unsigned binaries get `errSecMissingEntitlement`.
/// - The `security` CLI is an Apple-signed system binary that already has these
///   entitlements, and presents a Keychain access prompt to the user if needed.
///
/// # Returns
///
/// The raw JSON string from the Keychain, validated to contain
/// `claudeAiOauth.accessToken`.
///
/// # Errors
///
/// - If the `security` command is not found (not on macOS).
/// - If the Keychain entry doesn't exist (user hasn't logged in with `claude`).
/// - If the returned data is not valid JSON.
/// - If the JSON is missing the expected `claudeAiOauth.accessToken` field.
pub fn extract_credentials() -> Result<String> {
    // Determine the current username from environment variables.
    // macOS sets USER; some environments use LOGNAME instead.
    let username = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .context("Cannot determine username from USER or LOGNAME env vars")?;

    // Shell out to macOS `security` CLI to read the Keychain entry.
    // -s: service name (how Claude Code registers its credential)
    // -a: account name (the OS username)
    // -w: output only the password value (the JSON blob), not metadata
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-a",
            &username,
            "-w",
        ])
        .output()
        .context("Failed to execute `security` command. Are you on macOS?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Keychain access failed: {stderr}\n\
             Hint: Run `claude` on the host first to complete OAuth login."
        );
    }

    let creds = String::from_utf8(output.stdout)
        .context("Keychain returned non-UTF8 data")?
        .trim()
        .to_string();

    // Validate that the JSON has the expected structure before we pipe it
    // into a container. This catches the case where the Keychain entry exists
    // but contains unexpected data (e.g., from a different version of Claude Code).
    let parsed: serde_json::Value =
        serde_json::from_str(&creds).context("Keychain data is not valid JSON")?;

    parsed
        .get("claudeAiOauth")
        .and_then(|v| v.get("accessToken"))
        .context("Credential JSON missing claudeAiOauth.accessToken")?;

    Ok(creds)
}
