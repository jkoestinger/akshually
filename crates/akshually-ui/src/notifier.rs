//! The real Notifier: OS desktop notifications, through D-Bus (`notify-rust`) on Linux, where
//! `rune-notify` does nothing, and through `rune-notify` elsewhere (macOS needs the `.app`).

use akshually_core::{Notification, Notifier};

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
        rune_notify::post(&rune_notify::Notification::new(n.session.as_str(), n.title).body(n.body));
    }
}
