//! Persistence: one JSON file per open Session under the state directory, never inside the
//! repository under Review, plus one file for the Session bases recorded by `start`.
//!
//! The store mirrors the service's Sessions after every change: it rewrites the files whose
//! content changed and deletes the files of Sessions that ended. Whatever a Session serializes
//! is what survives a restart, so new Session state persists by being a serialized field.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Error, SessionId};

pub(crate) struct Store {
    dir: PathBuf,
    /// A hash of each file as last written, so unchanged Sessions aren't rewritten.
    written: HashMap<SessionId, u64>,
    /// The Session bases from `start`, which exist before their Session does.
    bases: PathBuf,
    bases_written: Option<u64>,
    /// The Inbox as last refreshed, and what the human has seen of it.
    inbox: PathBuf,
    inbox_written: Option<u64>,
}

impl Store {
    /// Opens the store under `state_dir`, creating it if needed, and loads its Sessions and
    /// Session bases. A file that can't be read is reported and left alone rather than
    /// stopping the app.
    pub(crate) fn open<S: DeserializeOwned, B: DeserializeOwned + Default>(
        state_dir: &Path,
        id: impl Fn(&S) -> &SessionId,
    ) -> Result<(Store, Vec<S>, B), Error> {
        let dir = state_dir.join("sessions");
        fs::create_dir_all(&dir)?;
        let bases_path = state_dir.join("session-bases.json");
        let mut store = Store {
            dir,
            written: HashMap::new(),
            bases: bases_path,
            bases_written: None,
            inbox: state_dir.join("inbox.json"),
            inbox_written: None,
        };
        let bases = match fs::read_to_string(&store.bases) {
            Ok(json) => match serde_json::from_str(&json) {
                Ok(bases) => {
                    store.bases_written = Some(hash(&json));
                    bases
                }
                Err(error) => {
                    eprintln!("akshually: skipping unreadable {}: {error}", store.bases.display());
                    B::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => B::default(),
            Err(e) => return Err(e.into()),
        };
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&store.dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let loaded = fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|json| Ok((serde_json::from_str::<S>(&json).map_err(|e| e.to_string())?, json)));
            match loaded {
                Ok((session, json)) => {
                    store.written.insert(id(&session).clone(), hash(&json));
                    sessions.push(session);
                }
                Err(error) => eprintln!("akshually: skipping unreadable Session {}: {error}", path.display()),
            }
        }
        Ok((store, sessions, bases))
    }

    /// Saves the Session bases, if they changed since last time.
    pub(crate) fn sync_bases<B: Serialize>(&mut self, bases: &B) -> Result<(), Error> {
        let json = serde_json::to_string(bases).map_err(std::io::Error::from)?;
        let hash = hash(&json);
        if self.bases_written != Some(hash) {
            write_atomically(&self.bases, &json)?;
            self.bases_written = Some(hash);
        }
        Ok(())
    }

    /// Reads the saved Inbox; the default when there is none or it can't be read (it's only a
    /// cache of the Forges, plus what the human has seen).
    pub(crate) fn load_inbox<I: DeserializeOwned + Default>(&mut self) -> I {
        let json = match fs::read_to_string(&self.inbox) {
            Ok(json) => json,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return I::default(),
            Err(error) => {
                eprintln!("akshually: skipping unreadable {}: {error}", self.inbox.display());
                return I::default();
            }
        };
        match serde_json::from_str(&json) {
            Ok(inbox) => {
                self.inbox_written = Some(hash(&json));
                inbox
            }
            Err(error) => {
                eprintln!("akshually: skipping unreadable {}: {error}", self.inbox.display());
                I::default()
            }
        }
    }

    /// Saves the Inbox, if it changed since last time.
    pub(crate) fn sync_inbox<I: Serialize>(&mut self, inbox: &I) -> Result<(), Error> {
        let json = serde_json::to_string(inbox).map_err(std::io::Error::from)?;
        let hash = hash(&json);
        if self.inbox_written != Some(hash) {
            write_atomically(&self.inbox, &json)?;
            self.inbox_written = Some(hash);
        }
        Ok(())
    }

    /// Makes the files match `sessions`: writes new and changed ones, deletes ended ones.
    pub(crate) fn sync<'a, S: Serialize + 'a>(
        &mut self,
        sessions: impl IntoIterator<Item = (&'a SessionId, &'a S)>,
    ) -> Result<(), Error> {
        let mut open = Vec::new();
        for (id, session) in sessions {
            open.push(id.clone());
            let json = serde_json::to_string(session).map_err(std::io::Error::from)?;
            let hash = hash(&json);
            if self.written.get(id) != Some(&hash) {
                self.write(id, &json)?;
                self.written.insert(id.clone(), hash);
            }
        }
        let ended: Vec<SessionId> = self.written.keys().filter(|id| !open.contains(id)).cloned().collect();
        for id in ended {
            match fs::remove_file(self.path(&id)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            self.written.remove(&id);
        }
        Ok(())
    }

    fn path(&self, id: &SessionId) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn write(&self, id: &SessionId, json: &str) -> Result<(), Error> {
        write_atomically(&self.path(id), json)
    }
}

/// Writes through a temporary file, so a crash leaves either the old file or the new one.
fn write_atomically(path: &Path, json: &str) -> Result<(), Error> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn hash(json: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    json.hash(&mut hasher);
    hasher.finish()
}
