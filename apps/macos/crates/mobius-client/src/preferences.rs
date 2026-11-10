//! The preferences file: the interface owns its shape, this module owns where it lives and
//! how it is replaced.

use anyhow::Context;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The stored preferences, or `T::default()` before the first save.
///
/// # Errors
///
/// When the file exists but cannot be read or decoded.
pub fn load<T: DeserializeOwned + Default>() -> anyhow::Result<T> {
    load_from(&path()?)
}

fn load_from<T: DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("decode preferences at {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("read preferences at {}", path.display())),
    }
}

/// Replaces the preferences file atomically.
fn save_to(path: &Path, preferences: &impl Serialize) -> anyhow::Result<()> {
    // Snapshot runs drive scripted states; they must never touch the user's preferences.
    if cfg!(feature = "snapshot") && std::env::var_os("MOBIUS_SNAPSHOT").is_some() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Missing preferences directory"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create preferences directory {}", parent.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary preferences in {}", parent.display()))?;
    file.write_all(&serde_json::to_vec_pretty(preferences).context("encode preferences")?)
        .with_context(|| format!("write preferences for {}", path.display()))?;
    file.as_file()
        .sync_all()
        .with_context(|| format!("sync preferences for {}", path.display()))?;
    file.persist(path)
        .with_context(|| format!("replace preferences at {}", path.display()))?;
    Ok(())
}

/// One queued change, and what to call it if saving fails.
struct Edit<T> {
    what: &'static str,
    apply: Box<dyn FnOnce(&mut T) + Send>,
}

/// Preference edits on their way to the file, so the interface never waits for a disk.
///
/// Edits apply in the order they were queued, to the file as it is when they are written,
/// and however many are waiting, one drain writes the file once. `drain` from a background
/// task keeps the interface responsive; `drain` on quit waits for a write in flight, then
/// writes whatever is left, so nothing is lost and nothing older lands on top.
pub struct Writer<T> {
    queue: Mutex<Vec<Edit<T>>>,
    /// Held from reading the file to replacing it, so two drains never interleave.
    writing: Mutex<()>,
}

impl<T> std::fmt::Debug for Writer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer").finish_non_exhaustive()
    }
}

impl<T> Default for Writer<T> {
    fn default() -> Self {
        Self {
            queue: Mutex::default(),
            writing: Mutex::default(),
        }
    }
}

impl<T: Serialize + DeserializeOwned + Default> Writer<T> {
    /// Adds `edit` behind the ones already waiting; `what` names it if saving fails.
    pub fn queue(&self, what: &'static str, edit: impl FnOnce(&mut T) + Send + 'static) {
        // A poisoned queue is never read; the next drain reports it.
        if !self.writing.is_poisoned()
            && let Ok(mut queue) = self.queue.lock()
        {
            queue.push(Edit {
                what,
                apply: Box::new(edit),
            });
        }
    }

    /// Whether another drain can process waiting edits.
    #[must_use]
    pub fn has_queued(&self) -> bool {
        !self.writing.is_poisoned() && self.queue.lock().is_ok_and(|queue| !queue.is_empty())
    }

    /// Writes every queued edit, in one write; without edits it touches nothing. Blocks
    /// until a drain already writing has finished.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, decoded or replaced. The message names what was being
    /// saved, those edits are not retried, and the next edit tries the file again.
    pub fn drain(&self) -> anyhow::Result<()> {
        self.drain_with(path)
    }

    fn drain_with(&self, path: impl FnOnce() -> anyhow::Result<PathBuf>) -> anyhow::Result<()> {
        let _writing = self.writing.lock().map_err(|_poisoned| poisoned())?;
        let edits = std::mem::take(&mut *self.queue.lock().map_err(|_poisoned| poisoned())?);
        if edits.is_empty() {
            return Ok(());
        }
        let mut names: Vec<&str> = Vec::new();
        for edit in &edits {
            if !names.contains(&edit.what) {
                names.push(edit.what);
            }
        }
        let written = path().and_then(|path| {
            let mut preferences: T = load_from(&path)?;
            for edit in edits {
                (edit.apply)(&mut preferences);
            }
            save_to(&path, &preferences)
        });
        written.with_context(|| format!("Could not save {}", names.join(", ")))
    }
}

fn poisoned() -> anyhow::Error {
    anyhow::anyhow!("preferences could not be saved after an earlier failure; restart the app")
}

fn path() -> anyhow::Result<PathBuf> {
    let home =
        std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("Home directory unavailable"))?;
    Ok(PathBuf::from(home).join("Library/Application Support/Mobius Desktop/preferences.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::cell::Cell;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    thread_local! {
        /// Every file write encodes the preferences once.
        static WRITES: Cell<usize> = const { Cell::new(0) };
    }

    #[derive(Default, Deserialize)]
    struct Order(Vec<u8>);

    impl Serialize for Order {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            WRITES.set(WRITES.get() + 1);
            self.0.serialize(serializer)
        }
    }

    fn stored(path: &Path) -> Vec<u8> {
        load_from::<Order>(path).unwrap().0
    }

    #[test]
    fn queued_edits_apply_in_order_to_the_stored_file_in_one_write() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("preferences.json");
        std::fs::write(&path, b"[9]").unwrap();
        let writer = Writer::<Order>::default();
        for value in [1, 2, 3] {
            writer.queue("order", move |order| order.0.push(value));
        }
        let at = || Ok(path.clone());
        writer.drain_with(at).unwrap();
        assert_eq!(stored(&path), [9, 1, 2, 3]);
        assert_eq!(WRITES.get(), 1);
        assert!(!writer.has_queued());
        writer.drain_with(at).unwrap();
        assert_eq!(WRITES.get(), 1, "nothing queued writes nothing");
    }

    #[test]
    fn a_flush_waits_for_the_write_in_flight_and_loses_no_edit() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("preferences.json");
        let writer = Arc::new(Writer::<Order>::default());
        let (entered, in_edit) = mpsc::channel();
        let (release, gate) = mpsc::channel::<()>();
        writer.queue("first", move |order| {
            entered.send(()).unwrap();
            gate.recv().unwrap();
            order.0.push(1);
        });
        let drain = |writer: &Arc<Writer<Order>>| {
            let (writer, path) = (Arc::clone(writer), path.clone());
            std::thread::spawn(move || writer.drain_with(|| Ok(path)))
        };
        let background = drain(&writer);
        in_edit.recv().unwrap();
        writer.queue("second", |order| order.0.push(2));
        let (flushed, flush_done) = mpsc::channel();
        let flush = {
            let (writer, path) = (Arc::clone(&writer), path.clone());
            std::thread::spawn(move || {
                let result = writer.drain_with(|| Ok(path));
                flushed.send(()).unwrap();
                result
            })
        };
        // Without waiting, the flush would write [2] and the older write would replace it.
        assert!(flush_done.recv_timeout(Duration::from_millis(100)).is_err());
        release.send(()).unwrap();
        background.join().unwrap().unwrap();
        flush.join().unwrap().unwrap();
        assert_eq!(stored(&path), [1, 2]);
    }

    #[test]
    fn a_poisoned_writer_does_not_request_an_endless_drain_loop() {
        let directory = tempfile::tempdir().expect("preferences directory");
        let path = directory.path().join("preferences.json");
        let writer = Arc::new(Writer::<Order>::default());
        writer.queue("broken edit", |_| panic!("test edit failed"));
        let failing = Arc::clone(&writer);
        assert!(
            std::thread::spawn(move || failing.drain_with(|| Ok(path)))
                .join()
                .is_err()
        );
        writer.queue("next edit", |order| order.0.push(1));
        assert!(
            !writer.has_queued(),
            "an unrecoverable writer must stop the drain loop"
        );
        assert!(
            writer
                .drain_with(|| panic!("a poisoned writer must fail before resolving its path"))
                .unwrap_err()
                .to_string()
                .contains("restart the app")
        );
    }

    #[test]
    fn a_failed_write_names_every_edit_and_the_next_one_tries_again() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("preferences.json");
        std::fs::write(&path, b"not json").unwrap();
        let writer = Writer::<Order>::default();
        writer.queue("the font", |order| order.0.push(1));
        writer.queue("window state", |order| order.0.push(2));
        writer.queue("the font", |order| order.0.push(3));
        let error = writer.drain_with(|| Ok(path.clone())).unwrap_err();
        assert!(
            format!("{error:#}").starts_with("Could not save the font, window state: decode"),
            "{error:#}"
        );
        assert!(!writer.has_queued());
        std::fs::write(&path, b"[]").unwrap();
        writer.queue("the font", |order| order.0.push(4));
        writer.drain_with(|| Ok(path.clone())).unwrap();
        assert_eq!(stored(&path), [4]);
    }
}
