//! The update notice (ADR 0003): a newer release of akshually on offer, installing it, and
//! restarting the app into it.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use super::{Inner, Service};
use crate::Error;
use crate::update::{Release, Version};

/// What the Window's update notice shows: `None` in [`super::WindowView`] while there is no
/// newer release, or the human dismissed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateView {
    /// The Release's version.
    pub version: Version,
    /// How far the Update has got, which says what the notice offers.
    pub stage: UpdateStage,
    /// The titles of the Sessions whose agent's `review` call is waiting: restarting ends
    /// those calls with an error, and the agents must run `review` again.
    pub waiting: Vec<String>,
}

/// How far the update has got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStage {
    /// A newer release exists: the notice offers Update.
    Available,
    /// Downloading and installing it.
    Installing,
    /// Installed, from the app or by `akshually update`: the notice offers Restart.
    Installed,
    /// Installing failed, for this reason; the notice offers Update again.
    Failed(String),
}

/// Relaunches the app as the installed binary. Returns only if that failed, with why.
type Relaunch = Arc<dyn Fn() -> io::Error + Send + Sync>;

/// The update as the service keeps it. Nothing of it is saved: a dismissal lasts until the
/// next launch, which checks again.
#[derive(Default)]
pub(super) struct Update {
    offer: Option<Offer>,
    dismissed: bool,
    relaunch: Option<Relaunch>,
}

struct Offer {
    version: Version,
    /// What to install; `None` once `akshually update` installed it instead.
    release: Option<Release>,
    stage: UpdateStage,
}

impl Inner {
    /// The update notice, unless dismissed.
    pub(super) fn update_view(&self) -> Option<UpdateView> {
        let offer = self.update.offer.as_ref().filter(|_| !self.update.dismissed)?;
        let waiting = self.sessions.iter().filter(|s| s.view.agent_waiting).map(|s| s.view.title()).collect();
        Some(UpdateView { version: offer.version.clone(), stage: offer.stage.clone(), waiting })
    }
}

impl Service {
    /// Looks for a newer release, unless the `check_for_updates` setting is off or the service
    /// has no updater, and offers it in the update notice. A failure is only logged: the human
    /// can't act on it. Blocks on the network.
    pub fn check_for_updates(&self) {
        let Some(updater) = &self.updater else { return };
        if !self.lock().settings.check_for_updates {
            return;
        }
        // Not holding the lock: GitHub may take a while.
        let release = match updater.newer() {
            Ok(Some(release)) => release,
            Ok(None) => return,
            Err(error) => {
                // Best-effort: the app works as well without knowing, and checks again tomorrow.
                eprintln!("akshually: couldn't look for a newer release: {error}");
                return;
            }
        };
        let mut inner = self.lock();
        // An Update installing or installed, or this one already offered (perhaps failed), stays.
        let keep = inner.update.offer.as_ref().is_some_and(|o| {
            o.version == release.version || matches!(o.stage, UpdateStage::Installing | UpdateStage::Installed)
        });
        if keep {
            return;
        }
        let version = release.version.clone();
        inner.update.offer = Some(Offer { version, release: Some(release), stage: UpdateStage::Available });
        inner.tell_window_subscribers();
    }

    /// Runs `check_for_updates` now, then every `every`, on a background thread, until the
    /// service is dropped.
    pub fn watch_updates(self: &Arc<Self>, every: Duration) {
        let service = Arc::downgrade(self);
        std::thread::spawn(move || {
            loop {
                let Some(svc) = service.upgrade() else { return };
                svc.check_for_updates();
                drop(svc);
                std::thread::sleep(every);
            }
        });
    }

    /// Human: installs the release the notice offers in place of this app's install; the
    /// notice then offers Restart, or says why it failed. Blocks on the network.
    pub fn install_update(&self) -> Result<(), Error> {
        let Some(updater) = &self.updater else { return Err(Error::NoUpdate) };
        let release = {
            let mut inner = self.lock();
            let Some(offer) = inner.update.offer.as_mut() else { return Err(Error::NoUpdate) };
            let (UpdateStage::Available | UpdateStage::Failed(_), Some(release)) = (&offer.stage, &offer.release)
            else {
                return Err(Error::NoUpdate);
            };
            let release = release.clone();
            offer.stage = UpdateStage::Installing;
            inner.tell_window_subscribers();
            release
        };
        // Not holding the lock: the download may take minutes.
        let installed = updater.install(&release);
        let mut inner = self.lock();
        if let Some(offer) = inner.update.offer.as_mut() {
            offer.stage = match &installed {
                Ok(()) => UpdateStage::Installed,
                Err(error) => UpdateStage::Failed(error.to_string()),
            };
        }
        inner.tell_window_subscribers();
        Ok(installed?)
    }

    /// `akshually update` installed `version` over this app's install: the notice offers
    /// Restart, even if the human dismissed it before.
    pub fn update_installed(&self, version: Version) {
        let mut inner = self.lock();
        inner.update.offer = Some(Offer { version, release: None, stage: UpdateStage::Installed });
        inner.update.dismissed = false;
        inner.tell_window_subscribers();
    }

    /// Human: hides the update notice until the next launch.
    pub fn dismiss_update(&self) {
        let mut inner = self.lock();
        inner.update.dismissed = true;
        inner.tell_window_subscribers();
    }

    /// The app: `relaunch` restarts it as the installed binary, returning only if it couldn't,
    /// with why. It runs outside the lock.
    pub fn on_restart(&self, relaunch: impl Fn() -> io::Error + Send + Sync + 'static) {
        self.lock().update.relaunch = Some(Arc::new(relaunch));
    }

    /// Human: restarts the app into the installed update. Agents' waiting `review` calls end
    /// with an error (ask first: [`UpdateView::waiting`]); Sessions come back from disk. Returns
    /// only if the app couldn't restart.
    pub fn restart(&self) -> Result<(), Error> {
        let relaunch = self.lock().update.relaunch.clone();
        let Some(relaunch) = relaunch else {
            return Err(Error::InvalidRequest("this app can't restart itself".into()));
        };
        Err(Error::Io(relaunch()))
    }
}
