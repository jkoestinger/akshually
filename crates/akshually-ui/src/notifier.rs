//! The real Notifier: OS desktop notifications, through D-Bus (`notify-rust`) on Linux, where
//! `rune-notify` does nothing, and through `rune-notify` elsewhere (macOS needs the `.app`).

use akshually_core::{Notification, Notifier};

/// The `rune-notify` id of a notification about the app itself, which belongs to no Session.
#[cfg(not(target_os = "linux"))]
const APP_NOTIFICATION_ID: &str = "akshually";

/// Shows each notification on the desktop. Never give it to a service in tests.
pub struct DesktopNotifier;

impl Notifier for DesktopNotifier {
    #[cfg(target_os = "linux")]
    fn notify(&self, n: Notification) {
        // The D-Bus call blocks, and the service is locked while it notifies.
        std::thread::spawn(move || {
            let _ = notify_rust::Notification::new().appname("akshually").summary(&n.title).body(&n.body).show();
        });
    }

    #[cfg(not(target_os = "linux"))]
    fn notify(&self, n: Notification) {
        // The id only makes a newer notification about the same thing replace the older one.
        // Clicking brings the app forward; no click opens a Session yet.
        let id = n.session.as_ref().map_or(APP_NOTIFICATION_ID, |s| s.as_str());
        rune_notify::post(&rune_notify::Notification::new(id, n.title).body(n.body));
    }
}
