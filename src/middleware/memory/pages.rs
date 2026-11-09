//! Memory pages on disk: one directory per scope, one Markdown file per page.
//!
//! ```text
//! <root>/global/<page>.md
//! <root>/s-<hash of owner key>/_scope.json   # the owner, for listing
//! <root>/s-<hash of owner key>/<page>.md
//! <root>/<scope>/.trash/<unix>-<id>-<page>.md # every replaced or deleted version, 30 days
//! <root>/<scope>/_log.md                      # one line per applied patch
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::Digest;

use super::{Basis, GLOBAL_DIR, MemoryOwner, key};
use crate::{Error, Result};

pub(super) const MAX_PAGES: usize = 200;
pub(super) const MAX_BODY_BYTES: usize = 16 * 1024;
pub(super) const MAX_DESCRIPTION_BYTES: usize = 200;
pub(super) const MAX_NAME_BYTES: usize = 64;
const TRASH_DIR: &str = ".trash";
const SCOPE_FILE: &str = "_scope.json";
const LOG_FILE: &str = "_log.md";
const TRASH_DAYS: u64 = 30;

/// One page: a named, described Markdown body that links to others with `[[name]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Page {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) body: String,
    pub(super) basis: Basis,
    pub(super) updated_at: u64,
    /// The chat that wrote a global page; scoped pages belong to their scope's owner.
    pub(super) author: Option<Arc<MemoryOwner>>,
}

impl Page {
    /// Every `[[name]]` with a valid page name, in order.
    pub(super) fn links(&self) -> impl Iterator<Item = &str> {
        self.body
            .split("[[")
            .skip(1)
            .filter_map(|rest| rest.split_once("]]").map(|(name, _)| name))
            .filter(|name| validate_name(name).is_ok())
    }

    fn render(&self) -> Result<String> {
        let mut text = format!(
            "---\ndescription: {}\nbasis: {}\nupdated_at: {}\n",
            self.description,
            self.basis.as_str(),
            self.updated_at
        );
        if let Some(author) = &self.author {
            text.push_str("author: ");
            text.push_str(&serde_json::to_string(author)?);
            text.push('\n');
        }
        text.push_str("---\n");
        text.push_str(&self.body);
        text.push('\n');
        Ok(text)
    }

    fn parse(name: &str, text: &str) -> std::result::Result<Self, String> {
        let rest = text.strip_prefix("---\n").ok_or("missing frontmatter")?;
        let (front, body) = rest
            .split_once("\n---\n")
            .ok_or("unterminated frontmatter")?;
        let (mut description, mut basis, mut updated_at, mut author) = (None, None, None, None);
        for line in front.lines() {
            let (field, value) = line.split_once(": ").ok_or("malformed frontmatter line")?;
            match field {
                "description" => description = Some(value.to_owned()),
                "basis" => basis = Some(Basis::parse(value).ok_or("unknown basis")?),
                "updated_at" => {
                    updated_at = Some(value.parse().map_err(|_parse| "bad updated_at")?);
                }
                "author" => {
                    author = Some(serde_json::from_str(value).map_err(|error| error.to_string())?);
                }
                _ => return Err(format!("unknown frontmatter field `{field}`")),
            }
        }
        Ok(Self {
            name: validate_name(name)?,
            description: validate_description(&description.ok_or("missing description")?)?,
            body: canonical_body(body)?,
            basis: basis.ok_or("missing basis")?,
            updated_at: updated_at.ok_or("missing updated_at")?,
            author,
        })
    }
}

/// Every page of one scope, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Scope {
    pub(super) owner: Option<Arc<MemoryOwner>>,
    pub(super) pages: Vec<Page>,
}

impl Scope {
    pub(super) fn dir_name(&self) -> Result<String> {
        dir_name(self.owner.as_deref())
    }

    pub(super) fn get(&self, name: &str) -> Option<&Page> {
        self.pages.iter().find(|page| page.name == name)
    }
}

fn dir_name(owner: Option<&MemoryOwner>) -> Result<String> {
    Ok(match owner {
        None => GLOBAL_DIR.into(),
        Some(owner) => {
            let digest = sha2::Sha256::digest(key(Some(owner))?.as_bytes());
            let hex = format!("{digest:x}").chars().take(32).collect::<String>();
            format!("s-{hex}")
        }
    })
}

pub(super) struct Pages {
    root: PathBuf,
}

impl Pages {
    pub(super) const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(super) async fn load(&self, owner: Option<&Arc<MemoryOwner>>) -> Result<Scope> {
        let dir = self.root.join(dir_name(owner.map(AsRef::as_ref))?);
        Ok(Scope {
            owner: owner.map(Arc::clone),
            pages: read_pages(&dir).await?,
        })
    }

    /// Every scope on disk, global first.
    pub(super) async fn all(&self) -> Result<Vec<Scope>> {
        let mut scopes = vec![self.load(None).await?];
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(scopes),
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let dir = entry.path();
            if !entry.file_type().await?.is_dir() || entry.file_name() == GLOBAL_DIR {
                continue;
            }
            let owner = match tokio::fs::read(dir.join(SCOPE_FILE)).await {
                Ok(bytes) => serde_json::from_slice::<MemoryOwner>(&bytes)
                    .map_err(|error| invalid(&dir, &error.to_string()))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if entry.file_name().to_str() != Some(&dir_name(Some(&owner))?) {
                return Err(invalid(&dir, "scope owner does not match its directory"));
            }
            scopes.push(Scope {
                owner: Some(Arc::new(owner)),
                pages: read_pages(&dir).await?,
            });
        }
        Ok(scopes)
    }

    /// The scope a host-facing page id (`<scope dir>/<page>`) names.
    pub(super) async fn locate(&self, id: &str) -> Result<(Scope, String)> {
        let missing = super::missing;
        let (dir, name) = id.split_once('/').ok_or_else(missing)?;
        let name = validate_name(name).map_err(Error::Tool)?;
        let scope = self
            .all()
            .await?
            .into_iter()
            .find(|scope| scope.dir_name().is_ok_and(|candidate| candidate == dir))
            .ok_or_else(missing)?;
        Ok((scope, name))
    }

    /// Writes `page` into `owner`'s scope, keeping any previous version in the trash.
    pub(super) async fn put(&self, owner: Option<&Arc<MemoryOwner>>, page: &Page) -> Result<()> {
        let dir = self.scope_dir(owner).await?;
        let path = dir.join(format!("{}.md", page.name));
        let temporary = dir.join(format!(".{}.md.tmp", page.name));
        tokio::fs::write(&temporary, page.render()?).await?;
        // Keep the live page in place until its complete replacement is ready.
        match tokio::fs::copy(&path, trash_path(&dir, &page.name)).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        tokio::fs::rename(&temporary, &path).await?;
        prune_trash(&dir).await?;
        Ok(())
    }

    /// Deletes `name` from `owner`'s scope into its trash.
    pub(super) async fn remove(&self, owner: Option<&Arc<MemoryOwner>>, name: &str) -> Result<()> {
        let dir = self.root.join(dir_name(owner.map(AsRef::as_ref))?);
        tokio::fs::create_dir_all(dir.join(TRASH_DIR)).await?;
        tokio::fs::rename(dir.join(format!("{name}.md")), trash_path(&dir, name)).await?;
        prune_trash(&dir).await
    }

    pub(super) async fn log(&self, owner: Option<&Arc<MemoryOwner>>, line: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let dir = self.scope_dir(owner).await?;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(LOG_FILE))
            .await?;
        file.write_all(format!("- {line}\n").as_bytes()).await?;
        Ok(())
    }

    async fn scope_dir(&self, owner: Option<&Arc<MemoryOwner>>) -> Result<PathBuf> {
        let dir = self.root.join(dir_name(owner.map(AsRef::as_ref))?);
        tokio::fs::create_dir_all(dir.join(TRASH_DIR)).await?;
        if let Some(owner) = owner {
            // Rewritten each time so a renamed project's label stays current.
            let temporary = dir.join("._scope.json.tmp");
            tokio::fs::write(&temporary, serde_json::to_vec(&**owner)?).await?;
            tokio::fs::rename(temporary, dir.join(SCOPE_FILE)).await?;
        }
        Ok(dir)
    }
}

fn trash_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(TRASH_DIR).join(format!(
        "{}-{}-{name}.md",
        super::now(),
        uuid::Uuid::new_v4().simple()
    ))
}

async fn prune_trash(dir: &Path) -> Result<()> {
    let now = super::now();
    let mut entries = tokio::fs::read_dir(dir.join(TRASH_DIR)).await?;
    while let Some(entry) = entries.next_entry().await? {
        let stamp = entry
            .file_name()
            .to_str()
            .and_then(|file| file.split_once('-'))
            .and_then(|(stamp, _)| stamp.parse::<u64>().ok());
        if stamp.is_some_and(|stamp| now.saturating_sub(stamp) > TRASH_DAYS * 86_400) {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}

async fn read_pages(dir: &Path) -> Result<Vec<Page>> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut pages = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .and_then(|file| file.to_str())
            .and_then(|file| file.strip_suffix(".md"))
            .filter(|name| !name.starts_with(['_', '.']))
        else {
            continue;
        };
        let text = tokio::fs::read_to_string(&path).await?;
        // Pages are plain files a person may edit; one unreadable page must not hide the rest.
        match Page::parse(name, &text) {
            Ok(page) => pages.push(page),
            Err(error) => tracing::warn!(path = %path.display(), %error, "skipping memory page"),
        }
    }
    pages.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(pages)
}

fn invalid(path: &Path, error: &str) -> Error {
    Error::Checkpoint(format!("invalid memory scope {}: {error}", path.display()))
}

pub(super) fn validate_name(name: &str) -> std::result::Result<String, String> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        });
    if valid {
        Ok(name.to_owned())
    } else {
        Err(format!(
            "a page name is 1–{MAX_NAME_BYTES} lowercase letters, digits and single hyphens: `{name}`"
        ))
    }
}

pub(super) fn validate_description(description: &str) -> std::result::Result<String, String> {
    let description = description.trim();
    if description.is_empty()
        || description.len() > MAX_DESCRIPTION_BYTES
        || description.contains(['\n', '\r'])
    {
        return Err(format!(
            "a page description is one line of 1–{MAX_DESCRIPTION_BYTES} UTF-8 bytes"
        ));
    }
    Ok(description.to_owned())
}

pub(super) fn canonical_body(body: &str) -> std::result::Result<String, String> {
    let body = body.replace("\r\n", "\n").replace('\r', "\n");
    let body = body.trim();
    if body.is_empty() || body.len() > MAX_BODY_BYTES {
        return Err(format!("a page body is 1–{MAX_BODY_BYTES} UTF-8 bytes"));
    }
    Ok(body.to_owned())
}
