//! Skills from a GitHub repository: the git trees API lists every file in one request
//! (`/git/trees/<ref>?recursive=1`), every folder holding a SKILL.md is a skill, and its
//! files come from raw.githubusercontent.com. Binary files (by extension, or not UTF-8)
//! and files over the size limit are skipped. `GITHUB_TOKEN`, when set, is sent along for
//! private repositories and a higher API rate limit.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};

use super::{build, frontmatter, store, valid_path, Skill, MAIN_FILE, MAX_SKILL_BYTES};
use crate::db::Db;

const API: &str = "https://api.github.com";
const RAW: &str = "https://raw.githubusercontent.com";
const TIMEOUT: Duration = Duration::from_secs(30);
/// Parallel downloads.
const PARALLEL: usize = 4;
const BINARY: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "tif", "tiff", "psd", "pdf", "zip", "gz", "tgz", "bz2", "xz",
    "7z", "tar", "jar", "rar", "woff", "woff2", "ttf", "otf", "eot", "mp3", "mp4", "wav", "ogg", "mov", "so", "dylib",
    "dll", "exe", "bin", "wasm", "pyc", "class", "sqlite", "db",
];

/// A repository, the ref to read and the folder to look in (empty: the whole repository).
#[derive(Debug, Clone, PartialEq)]
pub struct Repo {
    pub owner: String,
    pub repo: String,
    pub git_ref: String,
    pub path: String,
}

impl Repo {
    /// The `source` stored with a skill found in `dir`.
    pub fn source(&self, dir: &str) -> String {
        let dir = if dir.is_empty() { String::new() } else { format!("/{dir}") };
        format!("github:{}/{}{dir}@{}", self.owner, self.repo, self.git_ref)
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

fn valid_ref(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.contains("..")
        && !s.chars().any(|c| c.is_control() || " ~^:?*[\\".contains(c))
}

/// `https://github.com/<owner>/<repo>[/tree/<ref>/<path>]` (or `owner/repo`), with the
/// ref and folder given separately overriding the URL's. The default ref is `HEAD`.
pub fn parse_url(url: &str, git_ref: Option<&str>, path: Option<&str>) -> Result<Repo, String> {
    let url = url.trim().split(['?', '#']).next().unwrap_or_default().trim_end_matches('/');
    let rest = ["https://github.com/", "http://github.com/", "https://www.github.com/", "github.com/"]
        .iter()
        .find_map(|p| url.strip_prefix(p))
        .unwrap_or(url);
    if rest.contains("://") {
        return Err("only github.com repositories can be imported".into());
    }
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return Err("expected a repository URL like https://github.com/anthropics/skills".into());
    }
    let (owner, repo) = (parts[0], parts[1].strip_suffix(".git").unwrap_or(parts[1]));
    if !valid_name(owner) || !valid_name(repo) {
        return Err("invalid owner or repository name".into());
    }
    let (mut url_ref, mut url_path) = (None, String::new());
    if parts.len() >= 4 && (parts[2] == "tree" || parts[2] == "blob") {
        url_ref = Some(parts[3]);
        url_path = parts[4..].join("/");
        if url_path == MAIN_FILE || url_path.ends_with("/SKILL.md") {
            url_path = url_path.trim_end_matches(MAIN_FILE).trim_end_matches('/').to_string();
        }
    }
    let git_ref = git_ref.map(str::trim).filter(|r| !r.is_empty()).or(url_ref).unwrap_or("HEAD").to_string();
    if !valid_ref(&git_ref) {
        return Err(format!("invalid ref `{git_ref}`"));
    }
    let path = match path.map(|p| p.trim().trim_matches('/')).filter(|p| !p.is_empty()) {
        Some(p) => p.to_string(),
        None => url_path,
    };
    if !path.is_empty() && !valid_path(&path) {
        return Err(format!("invalid folder `{path}`"));
    }
    Ok(Repo { owner: owner.to_string(), repo: repo.to_string(), git_ref, path })
}

/// The repository and folder of a skill imported from GitHub (`source` as made by
/// [`Repo::source`]); `None` for skills from elsewhere.
pub fn parse_source(source: &str) -> Option<Repo> {
    let (repo_path, git_ref) = source.strip_prefix("github:")?.rsplit_once('@')?;
    let mut parts = repo_path.splitn(3, '/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    let path = parts.next().unwrap_or_default();
    Some(Repo { owner: owner.into(), repo: repo.into(), git_ref: git_ref.into(), path: path.into() })
}

/// One skill folder in a repository: its path, and its files (paths in the repository,
/// with sizes), SKILL.md included.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub dir: String,
    pub files: Vec<(String, usize)>,
}

fn prefix(dir: &str) -> String {
    if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    }
}

/// The skill folders under `root` among `blobs` (path, size). A file belongs to the
/// deepest skill folder containing it, so nested skills do not repeat their files.
pub fn group(blobs: &[(String, usize)], root: &str) -> Vec<Found> {
    let root = prefix(root);
    let under: Vec<&(String, usize)> = blobs.iter().filter(|(p, _)| p.starts_with(&root)).collect();
    let mut dirs: Vec<String> = under
        .iter()
        .filter_map(
            |(p, _)| if p == MAIN_FILE { Some(String::new()) } else { p.strip_suffix("/SKILL.md").map(str::to_string) },
        )
        .collect();
    dirs.sort();
    let prefixes: Vec<String> = dirs.iter().map(String::as_str).map(prefix).collect();
    let mut found: Vec<Found> = dirs.iter().map(|d| Found { dir: d.clone(), files: Vec::new() }).collect();
    for (path, size) in under {
        let owner = prefixes
            .iter()
            .enumerate()
            .filter(|(_, pre)| path.starts_with(pre.as_str()))
            .max_by_key(|(_, pre)| pre.len());
        if let Some((i, _)) = owner {
            found[i].files.push((path.clone(), *size));
        }
    }
    found
}

fn request(http: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    let mut rb = http
        .get(url)
        .timeout(TIMEOUT)
        .header(reqwest::header::USER_AGENT, concat!("zilong-router/", env!("CARGO_PKG_VERSION")))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json");
    if let Some(token) = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.trim().is_empty()) {
        rb = rb.bearer_auth(token.trim());
    }
    rb
}

async fn fetch(http: &reqwest::Client, url: &str) -> Result<reqwest::Response> {
    let resp = request(http, url).send().await.map_err(|e| anyhow!("GitHub: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let limited = resp.headers().get("x-ratelimit-remaining").and_then(|v| v.to_str().ok()) == Some("0");
    match status.as_u16() {
        403 | 429 if limited => bail!("GitHub rate limit reached; try again later or set GITHUB_TOKEN"),
        404 => {
            bail!("not found on GitHub (check the repository, ref and folder; private repositories need GITHUB_TOKEN)")
        }
        _ => bail!("GitHub answered {status}"),
    }
}

/// Percent-encodes a path for raw.githubusercontent.com, keeping the slashes.
fn encode(path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The skill folders of `repo`, and whether GitHub cut the file list short.
pub async fn scan(http: &reqwest::Client, repo: &Repo) -> Result<(Vec<Found>, bool)> {
    let url = format!("{API}/repos/{}/{}/git/trees/{}?recursive=1", repo.owner, repo.repo, encode(&repo.git_ref));
    let v: Value = fetch(http, &url).await?.json().await.map_err(|e| anyhow!("GitHub: {e}"))?;
    let blobs: Vec<(String, usize)> = v["tree"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["type"] == "blob")
        .filter_map(|e| Some((e["path"].as_str()?.to_string(), e["size"].as_u64().unwrap_or(0) as usize)))
        .collect();
    Ok((group(&blobs, &repo.path), v["truncated"] == true))
}

/// A file as text; `None` for binary content.
async fn text(http: &reqwest::Client, repo: &Repo, path: &str) -> Result<Option<String>> {
    let url = format!("{RAW}/{}/{}/{}/{}", repo.owner, repo.repo, encode(&repo.git_ref), encode(path));
    let bytes = fetch(http, &url).await?.bytes().await.map_err(|e| anyhow!("GitHub: {e}"))?;
    Ok(String::from_utf8(bytes.to_vec()).ok().filter(|t| !t.contains('\0')))
}

fn binary_name(path: &str) -> bool {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    BINARY.contains(&ext.as_str())
}

/// The SKILL.md, the text files (relative to the folder) and the files skipped, with why.
async fn download(
    http: &reqwest::Client,
    repo: &Repo,
    found: &Found,
) -> Result<(String, BTreeMap<String, String>, Vec<String>)> {
    let pre = prefix(&found.dir);
    let main = format!("{pre}{MAIN_FILE}");
    let skill_md = text(http, repo, &main).await?.ok_or_else(|| anyhow!("{main} is not a text file"))?;
    let (mut budget, mut skipped) = (MAX_SKILL_BYTES.saturating_sub(skill_md.len()), Vec::new());
    let mut wanted = Vec::new();
    for (path, size) in found.files.iter().filter(|(p, _)| *p != main) {
        let rel = path[pre.len()..].to_string();
        if binary_name(&rel) {
            skipped.push(format!("{rel} (binary)"));
        } else if !valid_path(&rel) {
            skipped.push(format!("{rel} (path)"));
        } else if *size > budget {
            skipped.push(format!("{rel} (over the 1 MB limit)"));
        } else {
            budget -= size;
            wanted.push((path.clone(), rel));
        }
    }
    let fetched: Vec<(String, Result<Option<String>>)> = futures_util::stream::iter(
        wanted.into_iter().map(|(path, rel)| async move { (rel, text(http, repo, &path).await) }),
    )
    .buffered(PARALLEL)
    .collect()
    .await;
    let mut files = BTreeMap::new();
    for (rel, r) in fetched {
        match r? {
            Some(t) => {
                files.insert(rel, t);
            }
            None => skipped.push(format!("{rel} (binary)")),
        }
    }
    Ok((skill_md, files, skipped))
}

/// The skills of `repo` for the import checklist: folder, name, description, file count.
pub async fn preview(http: &reqwest::Client, repo: &Repo) -> Result<Value> {
    let (found, truncated) = scan(http, repo).await?;
    let items: Vec<Value> = futures_util::stream::iter(found.iter().map(|f| async move {
        let main = format!("{}{MAIN_FILE}", prefix(&f.dir));
        let bytes: usize = f.files.iter().map(|(_, s)| s).sum();
        let mut item = json!({"dir": f.dir, "files": f.files.len(), "bytes": bytes});
        match text(http, repo, &main).await {
            Ok(Some(md)) => {
                let (fields, _) = frontmatter(&md);
                let folder = f.dir.rsplit('/').next().unwrap_or_default();
                let name = fields.get("name").cloned().filter(|n| !n.is_empty()).unwrap_or_else(|| folder.to_string());
                item["name"] = json!(name);
                item["description"] = json!(fields.get("description").cloned().unwrap_or_default());
            }
            Ok(None) => item["error"] = json!("SKILL.md is not a text file"),
            Err(e) => item["error"] = json!(format!("{e:#}")),
        }
        item
    }))
    .buffered(PARALLEL)
    .collect()
    .await;
    Ok(json!({
        "repo": format!("{}/{}", repo.owner, repo.repo), "ref": repo.git_ref, "path": repo.path,
        "truncated": truncated, "skills": items,
    }))
}

/// Adds the skills in folders `dirs` of `repo`: `{added, skipped, errors}`.
pub async fn import(http: &reqwest::Client, db: &Db, repo: &Repo, dirs: &[String]) -> Result<Value> {
    let (found, _) = scan(http, repo).await?;
    let (mut added, mut skipped_files, mut errors) = (Vec::new(), Vec::new(), Vec::new());
    for dir in dirs {
        let label = if dir.is_empty() { format!("{}/{}", repo.owner, repo.repo) } else { dir.clone() };
        let Some(f) = found.iter().find(|f| &f.dir == dir) else {
            errors.push(format!("{label}: no SKILL.md there"));
            continue;
        };
        let result = async {
            let (md, files, skipped) = download(http, repo, f).await.map_err(|e| format!("{e:#}"))?;
            let (skill, content) = build(&md, files, repo.source(dir), None, None)?;
            let skill = store(db, skill, &content, None)?;
            Ok::<_, String>((skill, skipped))
        }
        .await;
        match result {
            Ok((skill, skipped)) => {
                skipped_files.extend(skipped.into_iter().map(|s| format!("{}: {s}", skill.name)));
                added.push(skill.name);
            }
            Err(e) => errors.push(format!("{label}: {e}")),
        }
    }
    Ok(json!({"added": added, "skippedFiles": skipped_files, "errors": errors}))
}

/// Fetches a GitHub skill again; its switch and id stay, name and description come
/// from the new SKILL.md. Returns the skill and the files skipped.
pub async fn update(http: &reqwest::Client, db: &Db, old: &Skill) -> Result<(Skill, Vec<String>)> {
    let repo = parse_source(&old.source).ok_or_else(|| anyhow!("only skills imported from GitHub can be updated"))?;
    let (found, _) = scan(http, &repo).await?;
    let f =
        found.iter().find(|f| f.dir == repo.path).ok_or_else(|| anyhow!("{} has no SKILL.md any more", repo.path))?;
    let (md, files, skipped) = download(http, &repo, f).await?;
    let (skill, content) = build(&md, files, old.source.clone(), None, None).map_err(|e| anyhow!(e))?;
    let skill = store(db, skill, &content, Some(old)).map_err(|e| anyhow!(e))?;
    Ok((skill, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repository_urls() {
        let r = parse_url("https://github.com/anthropics/skills", None, None).unwrap();
        assert_eq!(
            (r.owner.as_str(), r.repo.as_str(), r.git_ref.as_str(), r.path.as_str()),
            ("anthropics", "skills", "HEAD", "")
        );
        let r = parse_url("https://github.com/anthropics/skills/tree/main/skills/pdf/", None, None).unwrap();
        assert_eq!((r.git_ref.as_str(), r.path.as_str()), ("main", "skills/pdf"));
        let r = parse_url("github.com/a/b.git", Some("v1"), Some("/x/y/")).unwrap();
        assert_eq!((r.repo.as_str(), r.git_ref.as_str(), r.path.as_str()), ("b", "v1", "x/y"));
        let r = parse_url("https://github.com/a/b/blob/dev/s/SKILL.md", None, None).unwrap();
        assert_eq!((r.git_ref.as_str(), r.path.as_str()), ("dev", "s"));
        assert_eq!(parse_url("a/b", None, None).unwrap().owner, "a");
        assert!(parse_url("https://gitlab.com/a/b", None, None).is_err());
        assert!(parse_url("https://github.com/a", None, None).is_err());
        assert!(parse_url("a/b", None, Some("../etc")).is_err());
        assert!(parse_url("a/b", Some("x y"), None).is_err());

        let source = r.source("skills/pdf");
        assert_eq!(source, "github:a/b/skills/pdf@dev");
        let back = parse_source(&source).unwrap();
        assert_eq!((back.owner.as_str(), back.path.as_str(), back.git_ref.as_str()), ("a", "skills/pdf", "dev"));
        assert_eq!(parse_source(&r.source("")).unwrap().path, "");
        assert!(parse_source("paste").is_none());
    }

    #[test]
    fn groups_files_by_skill_folder() {
        let blobs: Vec<(String, usize)> = [
            "README.md",
            "skills/pdf/SKILL.md",
            "skills/pdf/scripts/fill.py",
            "skills/pdf/forms/SKILL.md",
            "skills/pdf/forms/ref.md",
            "skills/docx/SKILL.md",
            "other/notes.md",
        ]
        .iter()
        .map(|p| (p.to_string(), 10))
        .collect();
        let found = group(&blobs, "");
        let dirs: Vec<&str> = found.iter().map(|f| f.dir.as_str()).collect();
        assert_eq!(dirs, ["skills/docx", "skills/pdf", "skills/pdf/forms"]);
        let files = |i: usize| found[i].files.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>();
        assert_eq!(files(1), ["skills/pdf/SKILL.md", "skills/pdf/scripts/fill.py"]);
        assert_eq!(files(2), ["skills/pdf/forms/SKILL.md", "skills/pdf/forms/ref.md"]);
        assert_eq!(group(&blobs, "skills/docx").len(), 1);
        assert!(group(&blobs, "other").is_empty());

        let root = vec![("SKILL.md".to_string(), 1), ("a.py".to_string(), 1)];
        assert_eq!(group(&root, ""), vec![Found { dir: String::new(), files: root.clone() }]);
        assert!(binary_name("fonts/A.TTF") && !binary_name("scripts/x.py") && !binary_name("LICENSE"));
        assert_eq!(encode("a b/ü.md"), "a%20b/%C3%BC.md");
    }
}
