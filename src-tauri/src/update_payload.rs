pub fn signed_payload(
    version: &str,
    published_at: &str,
    platform: &str,
    arch: &str,
    url: &str,
    sha256: &str,
) -> String {
    format!(
        "project-rx-launcher-update-v1\nversion={version}\npublishedAt={published_at}\nplatform={platform}\narch={arch}\nurl={url}\nsha256={}\n",
        sha256.to_ascii_lowercase()
    )
}
