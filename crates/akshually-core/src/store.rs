//! Persistence: one JSON file per open Session under the state directory, never inside the
//! repository under Review.
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
}

impl Store {
    /// Opens the store under `state_dir`, creating it if needed, and loads its Sessions.
    /// A file that can't be read is reported and left alone rather than stopping the app.
    pub(crate) fn open<S: DeserializeOwned>(
        state_dir: &Path,
        id: impl Fn(&S) -> &SessionId,
    ) -> Result<(Store, Vec<S>), Error> {
        let dir = state_dir.join("sessions");
        fs::create_dir_all(&dir)?;
        let mut store = Store { dir, written: HashMap::new() };
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
        Ok((store, sessions))
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

    /// Writes atomically, so a crash leaves either the old file or the new one.
    fn write(&self, id: &SessionId, json: &str) -> Result<(), Error> {
        let tmp = self.dir.join(format!("{id}.json.tmp"));
        let mut file = fs::File::create(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, self.path(id))?;
        Ok(())
    }
}

fn hash(json: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    json.hash(&mut hasher);
    hasher.finish()
}
