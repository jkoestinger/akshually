//! Seam A: what the core knows of each Forge, as Inbox rows name it: its display name, and which
//! host is its own public one.

use akshually_core::forge::ForgeKind;

#[test]
fn each_forge_has_its_display_name() {
    let names = [ForgeKind::GitHub.name(), ForgeKind::GitLab.name()];

    assert_eq!(names, ["GitHub", "GitLab"]);
}

#[test]
fn github_com_and_gitlab_com_are_their_forges_default_hosts_whatever_their_case() {
    let cases = [
        (ForgeKind::GitHub, "github.com", true),
        (ForgeKind::GitHub, "GitHub.com", true),
        (ForgeKind::GitLab, "gitlab.com", true),
        (ForgeKind::GitHub, "github.example.com", false),
        (ForgeKind::GitLab, "gitlab.example.com", false),
        (ForgeKind::GitHub, "gitlab.com", false),
    ];
    for (kind, host, default) in cases {
        assert_eq!(kind.is_default_host(host), default, "{host} on {}", kind.name());
    }
}
