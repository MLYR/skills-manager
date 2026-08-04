use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsShSkill {
    pub id: String,
    pub skill_id: String,
    pub name: String,
    pub source: String,
    pub installs: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum LeaderboardType {
    AllTime,
    Trending,
    Hot,
}

impl LeaderboardType {
    pub fn from_str(s: &str) -> Self {
        match s {
            "trending" => Self::Trending,
            "hot" => Self::Hot,
            _ => Self::AllTime,
        }
    }

    fn url(&self) -> &str {
        match self {
            Self::AllTime => "https://skills.sh/",
            Self::Trending => "https://skills.sh/trending",
            Self::Hot => "https://skills.sh/hot",
        }
    }
}

pub fn build_http_client(proxy_url: Option<&str>, timeout_secs: u64) -> reqwest::blocking::Client {
    let mut builder = reqwest::blocking::Client::builder()
        .user_agent("skills-manager")
        .timeout(std::time::Duration::from_secs(timeout_secs));
    if let Some(proxy) = proxy_url.filter(|s| !s.is_empty()) {
        if let Ok(p) = reqwest::Proxy::all(proxy) {
            builder = builder.proxy(p);
        }
    }
    builder.build().unwrap_or_default()
}

/// Check a repository's optional market manifest before cloning. A manifest is
/// authoritative only when it is readable and structurally valid; unavailable
/// manifests deliberately fall back to the normal Git install path.
pub fn source_manifest_skill_id(
    source: &str,
    skill_id: &str,
    proxy_url: Option<&str>,
) -> Option<Option<String>> {
    if !is_valid_github_source(source) || skill_id.trim().is_empty() {
        return None;
    }

    let client = build_http_client(proxy_url, 5);
    for branch in ["main", "master"] {
        let url =
            format!("https://raw.githubusercontent.com/{source}/{branch}/skills-manifest.json");
        let response = match client.get(url).send() {
            Ok(response) => response,
            Err(_) => return None,
        };
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        if !response.status().is_success() {
            return None;
        }
        let body = response.text().ok()?;
        return manifest_skill_id(&body, source, skill_id);
    }

    None
}

fn manifest_skill_id(body: &str, source: &str, skill_id: &str) -> Option<Option<String>> {
    let manifest: serde_json::Value = serde_json::from_str(body).ok()?;
    if manifest.get("source")?.as_str()? != source {
        return None;
    }
    let skills = manifest.get("skills")?.as_object()?;
    if skills.contains_key(skill_id) {
        return Some(Some(skill_id.to_string()));
    }

    // Renames are publisher-owned data. Only follow an explicit manifest
    // alias whose destination is also present in the same manifest; never
    // infer a replacement from fuzzy names or install counts.
    let replacement = manifest
        .get("aliases")
        .and_then(|value| value.as_object())
        .and_then(|aliases| aliases.get(skill_id))
        .and_then(|value| value.as_str())
        .filter(|candidate| skills.contains_key(*candidate));
    Some(replacement.map(str::to_string))
}

pub fn is_valid_github_source(source: &str) -> bool {
    let mut parts = source.split('/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let Some(repository) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !owner.is_empty()
        && !repository.is_empty()
        && [owner, repository].into_iter().all(|part| {
            part != "."
                && part != ".."
                && part.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
                })
        })
}

pub fn fetch_leaderboard(
    board: LeaderboardType,
    proxy_url: Option<&str>,
) -> Result<Vec<SkillsShSkill>> {
    let client = build_http_client(proxy_url, 15);

    let html = client
        .get(board.url())
        .send()
        .context("Failed to fetch skills.sh")?
        .text()
        .context("Failed to read response")?;

    parse_leaderboard_html(&html)
}

fn parse_leaderboard_html(html: &str) -> Result<Vec<SkillsShSkill>> {
    if let Ok(skills) = parse_next_data(html) {
        if !skills.is_empty() {
            return Ok(skills);
        }
    }

    let skills = parse_embedded_skill_objects(html)?;
    if skills.is_empty() {
        log::warn!("Could not find skills in skills.sh HTML");
    }
    Ok(skills)
}

fn parse_next_data(html: &str) -> Result<Vec<SkillsShSkill>> {
    let marker = r#"<script id="__NEXT_DATA__" type="application/json">"#;
    let start = html
        .find(marker)
        .ok_or_else(|| anyhow::anyhow!("__NEXT_DATA__ not found"))?
        + marker.len();

    let end = html[start..]
        .find("</script>")
        .ok_or_else(|| anyhow::anyhow!("Closing script tag not found"))?
        + start;

    let json_str = &html[start..end];
    let data: serde_json::Value =
        serde_json::from_str(json_str).context("Failed to parse __NEXT_DATA__ JSON")?;

    let skills_array = data
        .pointer("/props/pageProps/initialSkills")
        .or_else(|| data.pointer("/props/pageProps/skills"))
        .or_else(|| data.pointer("/props/pageProps/items"))
        .and_then(|v| v.as_array());

    match skills_array {
        Some(arr) => Ok(parse_skills_array(arr)),
        None => Ok(Vec::new()),
    }
}

fn parse_skills_array(arr: &[serde_json::Value]) -> Vec<SkillsShSkill> {
    let mut seen = HashSet::new();
    let mut skills = Vec::new();

    for item in arr {
        let source = item
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let skill_id = item
            .get("skillId")
            .or_else(|| item.get("skill_id"))
            .or_else(|| item.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if source.is_empty() || skill_id.is_empty() {
            continue;
        }

        let id = format!("{}/{}", source, skill_id);
        if !seen.insert(id.clone()) {
            continue;
        }

        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .unwrap_or(&skill_id)
            .to_string();
        let installs = item.get("installs").and_then(|v| v.as_u64()).unwrap_or(0);

        skills.push(SkillsShSkill {
            id,
            skill_id,
            name,
            source,
            installs,
        });
    }

    skills
}

fn parse_embedded_skill_objects(html: &str) -> Result<Vec<SkillsShSkill>> {
    let pattern = Regex::new(
        r#"(?:\\)?\"source(?:\\)?\":(?:\\)?\"(?P<source>[^"\\]+)(?:\\)?\",(?:[^{}]|\\.)*?(?:(?:\\)?\"skillId(?:\\)?\"|(?:\\)?\"skill_id(?:\\)?\"):(?:\\)?\"(?P<skill_id>[^"\\]+)(?:\\)?\",(?:[^{}]|\\.)*?(?:\\)?\"name(?:\\)?\":(?:\\)?\"(?P<name>[^"\\]*)(?:\\)?\",(?:[^{}]|\\.)*?(?:\\)?\"installs(?:\\)?\":(?P<installs>\d+)"#,
    )
    .context("Failed to build skills.sh regex")?;

    let fallback_pattern = Regex::new(
        r#"\{"source":"(?P<source>[^"]+)","skill_id":"(?P<skill_id>[^"]+)"(?:,"name":"(?P<name>[^"]*)")?(?:.*?"installs":(?P<installs>\d+))?\}"#,
    )
    .context("Failed to build fallback skills.sh regex")?;

    let mut skills = parse_embedded_with_regex(html, &pattern);
    if skills.is_empty() {
        skills = parse_embedded_with_regex(html, &fallback_pattern);
    }

    Ok(skills)
}

fn parse_embedded_with_regex(html: &str, pattern: &Regex) -> Vec<SkillsShSkill> {
    let mut seen = HashSet::new();
    let mut skills = Vec::new();

    for caps in pattern.captures_iter(html) {
        let source = match caps.name("source") {
            Some(v) => v.as_str().replace(r#"\""#, "\""),
            None => continue,
        };
        let skill_id = match caps.name("skill_id") {
            Some(v) => v.as_str().replace(r#"\""#, "\""),
            None => continue,
        };

        let id = format!("{}/{}", source, skill_id);
        if !seen.insert(id.clone()) {
            continue;
        }

        let name = caps
            .name("name")
            .map(|v| v.as_str().replace(r#"\""#, "\""))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| skill_id.clone());
        let installs = caps
            .name("installs")
            .and_then(|v| v.as_str().parse::<u64>().ok())
            .unwrap_or(0);

        skills.push(SkillsShSkill {
            id,
            skill_id,
            name,
            source,
            installs,
        });
    }

    skills
}

pub fn search_skills(
    query: &str,
    limit: usize,
    proxy_url: Option<&str>,
) -> Result<Vec<SkillsShSkill>> {
    let client = build_http_client(proxy_url, 15);

    let url = format!(
        "https://skills.sh/api/search?q={}&limit={}",
        urlencoding::encode(query),
        limit
    );

    let resp: serde_json::Value = client
        .get(&url)
        .send()
        .context("Failed to search skills.sh")?
        .json()
        .context("Failed to parse search response")?;

    if let Some(arr) = resp.as_array() {
        return Ok(parse_skills_array(arr));
    }

    let skills_array = resp.get("skills").and_then(|v| v.as_array());
    match skills_array {
        Some(arr) => Ok(parse_skills_array(arr)),
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_valid_github_source, manifest_skill_id, parse_embedded_skill_objects,
        parse_next_data,
    };

    #[test]
    fn parses_legacy_next_data_payload() {
        let html = r#"
        <html>
          <script id="__NEXT_DATA__" type="application/json">
            {"props":{"pageProps":{"initialSkills":[{"source":"antfu/skills","skillId":"vite","name":"vite","installs":152}]}}}
          </script>
        </html>
        "#;

        let skills = parse_next_data(html).expect("legacy payload should parse");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "antfu/skills/vite");
    }

    #[test]
    fn parses_current_rsc_payload() {
        let html = r#"
        <script>self.__next_f.push([1,"...\n[{\"source\":\"anthropics/skills\",\"skillId\":\"template-skill\",\"name\":\"template-skill\",\"installs\":238},{\"source\":\"vercel/ai\",\"skillId\":\"ai-sdk\",\"name\":\"ai-sdk\",\"installs\":265}]...\n"])</script>
        "#;

        let skills = parse_embedded_skill_objects(html).expect("rsc payload should parse");
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].id, "anthropics/skills/template-skill");
        assert_eq!(skills[1].id, "vercel/ai/ai-sdk");
    }

    #[test]
    fn parses_legacy_embedded_payload() {
        let html = r#"
        {"source":"openai/skills","skill_id":"playwright","name":"playwright","installs":2}
        "#;

        let skills = parse_embedded_skill_objects(html).expect("legacy fallback should parse");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "openai/skills/playwright");
    }

    #[test]
    fn manifest_check_detects_removed_market_skill() {
        let manifest = r#"{
            "source": "acme/skills",
            "skills": { "available": { "files": 1 } }
        }"#;

        assert_eq!(
            manifest_skill_id(manifest, "acme/skills", "available"),
            Some(Some("available".to_string()))
        );
        assert_eq!(manifest_skill_id(manifest, "acme/skills", "removed"), Some(None));
    }

    #[test]
    fn manifest_check_only_follows_explicit_valid_aliases() {
        let manifest = r#"{
            "source": "acme/skills",
            "skills": { "new-name": { "files": 1 } },
            "aliases": { "old-name": "new-name", "missing": "not-published" }
        }"#;

        assert_eq!(
            manifest_skill_id(manifest, "acme/skills", "old-name"),
            Some(Some("new-name".to_string()))
        );
        assert_eq!(manifest_skill_id(manifest, "acme/skills", "missing"), Some(None));
    }

    #[test]
    fn source_manifest_only_accepts_owner_repository_pairs() {
        assert!(is_valid_github_source("heygen-com/hyperframes"));
        assert!(!is_valid_github_source("heygen-com/hyperframes/extra"));
        assert!(!is_valid_github_source("../hyperframes"));
    }
}
