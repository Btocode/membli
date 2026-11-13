//! Checks GitHub for a newer Membli release.

pub const REPO: &str = "Btocode/membli";

pub struct Release {
    pub tag: String,
    pub url: String,
}

fn parse(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split(|c: char| c == '.' || c == '-').map(|x| x.parse().unwrap_or(0)).collect()
}

/// `Ok(Some)` when a newer release exists, `Ok(None)` when this is the newest.
pub fn check() -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = ureq::get(&url)
        .header("User-Agent", "membli-update-check")
        .header("Accept", "application/vnd.github+json")
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .call()
        .map_err(|e| format!("Could not reach GitHub: {e}"))?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    if resp.status().as_u16() != 200 {
        return Err(format!("GitHub answered {}", resp.status()));
    }
    let v: serde_json::Value = resp.into_body().read_json().map_err(|e| e.to_string())?;
    let tag = v.get("tag_name").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let page = v.get("html_url").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if !tag.is_empty() && parse(&tag) > parse(env!("CARGO_PKG_VERSION")) {
        Ok(Some(Release { tag, url: page }))
    } else {
        Ok(None)
    }
}
