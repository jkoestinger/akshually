//! The diff layout: how the window shows every Session's Changeset, stacked or side by side.

use serde::{Deserialize, Serialize};

use super::Service;

/// How the window lays out a Changeset's old and new lines. It applies to every Session's tab
/// at once, and survives a restart when the service has a state directory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLayout {
    /// Old and new lines interleaved in one column.
    #[default]
    Stacked,
    /// Old lines on the left, new lines on the right.
    SideBySide,
}

impl Service {
    /// How the window lays out every Session's Changeset.
    pub fn diff_layout(&self) -> DiffLayout {
        self.lock().diff_layout
    }

    /// Human: lays out every Session's Changeset as `layout`, and saves it if the service has a
    /// store.
    pub fn set_diff_layout(&self, layout: DiffLayout) {
        let mut inner = self.lock();
        inner.diff_layout = layout;
        // The window already shows the new layout; a failed save only loses it at restart.
        if let Some(store) = &inner.store
            && let Err(error) = store.save_diff_layout(&layout)
        {
            eprintln!("akshually: couldn't save the diff layout: {error}");
        }
    }
}
