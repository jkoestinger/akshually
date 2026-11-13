//! The Window: whether it displays the Inbox or one Session, its Diff layout, and the one
//! subscription that tells the UI all of it.

use serde::{Deserialize, Serialize};

use super::{InboxView, Inner, Service, SessionId, SessionView};
use crate::Error;

/// Everything the Window shows, as one snapshot: what the UI renders, whole, after each change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WindowView {
    /// Every Session, in the order their tabs show.
    pub sessions: Vec<SessionView>,
    pub displayed: Displayed,
    pub inbox: InboxView,
    pub diff_layout: DiffLayout,
}

/// What the Window displays. It always displays something: the Inbox at startup, and whenever
/// the Session it displays goes away.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Displayed {
    Session(SessionId),
    #[default]
    Inbox,
}

/// How the Window lays out a Changeset's old and new lines. It applies to every Session at once,
/// and survives a restart when the service has a state directory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLayout {
    /// Old and new lines interleaved in one column.
    #[default]
    Stacked,
    /// Old lines on the left, new lines on the right.
    SideBySide,
}

type WindowSubscriber = Box<dyn Fn(&WindowView) + Send>;

impl Displayed {
    /// Whether this is the Session `id`.
    pub fn is_session(&self, id: &SessionId) -> bool {
        matches!(self, Displayed::Session(displayed) if displayed == id)
    }
}

/// The Window as the service keeps it, beside its Sessions and the Inbox.
#[derive(Default)]
pub(super) struct Window {
    displayed: Displayed,
    /// The human opened the Inbox, rather than the Window falling back to it at startup or when
    /// its Session went away; only ever set while it displays the Inbox.
    inbox_opened: bool,
    pub(super) diff_layout: DiffLayout,
    subscribers: Vec<WindowSubscriber>,
}

impl Window {
    /// Displays the Session `id`, without telling anyone yet.
    pub(super) fn display_session(&mut self, id: &SessionId) {
        self.displayed = Displayed::Session(id.clone());
        self.inbox_opened = false;
    }

    /// Whether the human has the Inbox open. Only then does it keep the view from a Session
    /// that becomes the human's Turn, and a new review request doesn't mark it.
    pub(super) fn inbox_opened(&self) -> bool {
        self.inbox_opened
    }
}

impl Inner {
    fn window_view(&self) -> WindowView {
        WindowView {
            sessions: self.views(),
            displayed: self.window.displayed.clone(),
            inbox: self.inbox.view(&self.sessions),
            diff_layout: self.window.diff_layout,
        }
    }

    /// Gives the Window subscribers the Window as it is now.
    pub(super) fn tell_window_subscribers(&self) {
        if self.window.subscribers.is_empty() {
            return;
        }
        let window = self.window_view();
        for subscriber in &self.window.subscribers {
            subscriber(&window);
        }
    }

    /// The Session at `at` became the human's Turn. It takes the view only from an Inbox the
    /// Window fell back to; otherwise, unless it's displayed, it gets attention.
    pub(super) fn humans_turn_display(&mut self, at: usize) {
        let id = self.sessions[at].view.id.clone();
        let attention = match &self.window.displayed {
            Displayed::Inbox if self.window.inbox_opened => true,
            Displayed::Inbox => {
                self.window.display_session(&id);
                false
            }
            Displayed::Session(displayed) => displayed != &id,
        };
        self.sessions[at].view.attention = attention;
    }

    /// The Session `id` went away: if the Window displayed it, it displays the Inbox.
    pub(super) fn session_gone(&mut self, id: &SessionId) {
        if self.window.displayed.is_session(id) {
            self.window.displayed = Displayed::Inbox;
        }
    }
}

impl Service {
    /// Calls `f` with the whole Window after each change: a Session changed, opened or went
    /// away, the Inbox refreshed or its attention changed, the Diff layout was set, or something
    /// else is displayed. `f` runs while the service is locked, so it must hand the Window off
    /// (to a channel, a UI thread) rather than call back in.
    pub fn subscribe_window(&self, f: impl Fn(&WindowView) + Send + 'static) {
        self.lock().window.subscribers.push(Box::new(f));
    }

    /// The Window as it is now.
    pub fn window(&self) -> WindowView {
        self.lock().window_view()
    }

    /// Human: opens the Session's tab, which clears its attention.
    pub fn display(&self, id: &SessionId) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        if inner.window.displayed.is_session(id) && !inner.sessions[at].view.attention {
            return Ok(());
        }
        inner.window.display_session(id);
        inner.sessions[at].view.attention = false;
        inner.notify();
        Ok(())
    }

    /// Human: opens the Inbox tab, which clears its attention. Sessions that need the human
    /// meanwhile get attention rather than the view.
    pub fn display_inbox(&self) {
        let mut inner = self.lock();
        let already = std::mem::replace(&mut inner.window.inbox_opened, true);
        inner.window.displayed = Displayed::Inbox;
        // The Inbox is saved only when its attention changed: what's displayed isn't saved.
        if inner.inbox.clear_attention() {
            inner.notify_inbox();
        } else if !already {
            inner.tell_window_subscribers();
        }
    }

    /// Human: lays out every Session's Changeset as `layout`, and saves it if the service has a
    /// store.
    pub fn set_diff_layout(&self, layout: DiffLayout) {
        let mut inner = self.lock();
        if inner.window.diff_layout == layout {
            return;
        }
        inner.window.diff_layout = layout;
        // The Window shows the new layout either way; a failed save only loses it at restart.
        if let Some(store) = &inner.store
            && let Err(error) = store.save_diff_layout(&layout)
        {
            eprintln!("akshually: couldn't save the Diff layout: {error}");
        }
        inner.tell_window_subscribers();
    }
}
