//! Memory pages on disk: one directory per scope, one Markdown file per page.
//!
//! ```text
//! <root>/global/<page>.md
//! <root>/s-<hash of owner key>/_scope.json   # the owner, for listing
//! <root>/s-<hash of owner key>/<page>.md
//! <root>/<scope>/.trash/<unix>-<id>-<page>.md # every replaced or deleted version, 30 days
//! <root>/<scope>/_log.md                      # one line per applied patch
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::Digest as _;

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
/// Scopes whose parsed pages stay in memory; a gateway sees one per Bot and project.
const MAX_CACHED_SCOPES: usize = 64;

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
    pub(super) digest: Digest,
}

/// A page's content digest, worked out on first use. It never decides page equality, and a
/// copy starts empty, so a page changed after cloning is hashed afresh.
#[derive(Debug, Default)]
pub(super) struct Digest(std::sync::OnceLock<String>);

impl Clone for Digest {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for Digest {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for Digest {}

impl Page {
    /// The version the prompt projection compares: the write stamp and a digest of what the
    /// model reads, so a hand edit that keeps the stamp still counts as a change.
    pub(super) fn version(&self) -> (u64, String) {
        let digest = self.digest.0.get_or_init(|| {
            let mut hash = sha2::Sha256::new();
            hash.update(self.description.len().to_be_bytes());
            hash.update(self.description.as_bytes());
            hash.update(self.body.len().to_be_bytes());
            hash.update(self.body.as_bytes());
            hash.update(self.basis.as_str().as_bytes());
            format!("{:x}", hash.finalize())
        });
        (self.updated_at, digest.clone()) // rules-allow: the projection records its own copy
    }

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
            digest: Digest::default(),
        })
    }
}

/// Every page of one scope, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Scope {
    pub(super) owner: Option<Arc<MemoryOwner>>,
    /// Shared with the page cache, so loading an unchanged scope copies nothing.
    pub(super) pages: Arc<[Page]>,
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
    cache: Mutex<Cache>,
}

/// Parsed scopes by directory, each with the file listing it was read from.
#[derive(Default)]
struct Cache {
    scopes: HashMap<PathBuf, Cached>,
    clock: u64,
}

impl Cache {
    /// `dir`'s pages when they were read from exactly `files`.
    fn hit(&mut self, dir: &Path, files: &[PageFile]) -> Option<Arc<[Page]>> {
        self.clock += 1;
        let cached = self
            .scopes
            .get_mut(dir)
            .filter(|cached| cached.files == files)?;
        cached.used = self.clock;
        Some(Arc::clone(&cached.pages))
    }

    fn insert(&mut self, dir: &Path, files: Vec<PageFile>, pages: Arc<[Page]>) {
        // ponytail: evicts by scanning for the least recently used; 64 entries keep it cheap.
        if self.scopes.len() >= MAX_CACHED_SCOPES && !self.scopes.contains_key(dir) {
            self.scopes.retain({
                let oldest = self.scopes.values().map(|cached| cached.used).min();
                move |_, cached| Some(cached.used) != oldest
            });
        }
        let used = self.clock;
        self.scopes
            .insert(dir.to_path_buf(), Cached { files, pages, used });
    }
}

struct Cached {
    files: Vec<PageFile>,
    pages: Arc<[Page]>,
    used: u64,
}

impl Pages {
    pub(super) fn new(root: PathBuf) -> Self {
        Self {
            root,
            cache: Mutex::default(),
        }
    }

    pub(super) async fn load(&self, owner: Option<&Arc<MemoryOwner>>) -> Result<Scope> {
        let dir = self.root.join(dir_name(owner.map(AsRef::as_ref))?);
        Ok(Scope {
            owner: owner.map(Arc::clone),
            pages: self.scope_pages(&dir).await?,
        })
    }

    /// `dir`'s pages, re-read only when a page file was added, removed, resized or touched,
    /// which also catches a person editing a page by hand.
    async fn scope_pages(&self, dir: &Path) -> Result<Arc<[Page]>> {
        let files = page_files(dir).await?;
        let hit = self.cache()?.hit(dir, &files);
        if let Some(pages) = hit {
            return Ok(pages);
        }
        let pages: Arc<[Page]> = read_pages(&files).await?.into();
        self.cache()?.insert(dir, files, Arc::clone(&pages));
        Ok(pages)
    }

    fn cache(&self) -> Result<std::sync::MutexGuard<'_, Cache>> {
        self.cache
            .lock()
            .map_err(|_poisoned| Error::Checkpoint("memory page cache is poisoned".into()))
    }

    /// Drops `dir`'s cached pages after this process changed them.
    fn forget(&self, dir: &Path) -> Result<()> {
        self.cache()?.scopes.remove(dir);
        Ok(())
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
                pages: self.scope_pages(&dir).await?,
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
        self.forget(&dir)?;
        prune_trash(&dir).await?;
        Ok(())
    }

    /// Deletes `name` from `owner`'s scope into its trash.
    pub(super) async fn remove(&self, owner: Option<&Arc<MemoryOwner>>, name: &str) -> Result<()> {
        let dir = self.root.join(dir_name(owner.map(AsRef::as_ref))?);
        tokio::fs::create_dir_all(dir.join(TRASH_DIR)).await?;
        tokio::fs::rename(dir.join(format!("{name}.md")), trash_path(&dir, name)).await?;
        self.forget(&dir)?;
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

/// One page file as listed: enough to tell whether it changed since it was read.
#[derive(Debug, PartialEq, Eq)]
struct PageFile {
    name: String,
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

/// The page files in `dir`, by name; none when the scope has no directory yet.
async fn page_files(dir: &Path) -> Result<Vec<PageFile>> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .and_then(|file| file.to_str())
            .and_then(|file| file.strip_suffix(".md"))
            .filter(|name| !name.starts_with(['_', '.']))
            .map(str::to_owned)
        else {
            continue;
        };
        let metadata = entry.metadata().await?;
        files.push(PageFile {
            name,
            path,
            len: metadata.len(),
            modified: metadata.modified().ok(),
        });
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(files)
}

async fn read_pages(files: &[PageFile]) -> Result<Vec<Page>> {
    let mut pages = Vec::new();
    for file in files {
        let text = tokio::fs::read_to_string(&file.path).await?;
        // Pages are plain files a person may edit; one unreadable page must not hide the rest.
        match Page::parse(&file.name, &text) {
            Ok(page) => pages.push(page),
            Err(error) => {
                tracing::warn!(path = %file.path.display(), %error, "skipping memory page");
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn page(name: &str, body: &str) -> Page {
        Page {
            name: name.into(),
            description: format!("about {name}"),
            body: body.into(),
            basis: Basis::AgentObservation,
            updated_at: 1,
            author: None,
            digest: Digest::default(),
        }
    }

    #[tokio::test]
    async fn unchanged_scopes_come_from_the_cache_and_hand_edits_are_reread() {
        let root = tempfile::tempdir().expect("temporary directory");
        let pages = Pages::new(root.path().to_path_buf());
        pages
            .put(None, &page("deploy", "make ship"))
            .await
            .expect("put");
        let first = pages.load(None).await.expect("load").pages;
        let second = pages.load(None).await.expect("load").pages;
        assert!(
            Arc::ptr_eq(&first, &second),
            "an unchanged scope is not re-read"
        );

        let file = root.path().join(GLOBAL_DIR).join("deploy.md");
        let text = std::fs::read_to_string(&file).expect("page file");
        std::fs::write(&file, text.replace("make ship", "make ship --prod")).expect("hand edit");
        let edited = pages.load(None).await.expect("load").pages;
        assert_eq!(
            edited.first().map(|page| page.body.as_str()),
            Some("make ship --prod")
        );
    }

    #[test]
    fn the_cache_keeps_at_most_its_cap_dropping_the_least_recently_used() {
        let mut cache = Cache::default();
        let dir = |index: usize| PathBuf::from(format!("scope-{index}"));
        for index in 0..=MAX_CACHED_SCOPES {
            if index == MAX_CACHED_SCOPES {
                // Touch the first scope so the second becomes the oldest.
                assert!(cache.hit(&dir(0), &[]).is_some());
            }
            cache.clock += 1;
            cache.insert(&dir(index), Vec::new(), Arc::default());
        }
        assert_eq!(cache.scopes.len(), MAX_CACHED_SCOPES);
        assert!(cache.scopes.contains_key(&dir(0)));
        assert!(!cache.scopes.contains_key(&dir(1)));
    }
}
