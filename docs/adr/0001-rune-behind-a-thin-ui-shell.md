# Rune for the UI, behind a thin shell

We build the desktop UI with [Rune](https://github.com/anomalyco/rune), even though it is pre-release (0.0.1, no releases, API changing daily), because its kit ships a virtualised diff view with syntax highlighting, split/unified modes and per-line comment hooks, which is most of this app's UI. To contain the risk, all review logic (Sessions, Rounds, Threads, Sources, the CLI-to-window channel) lives in a UI-free core crate, so the Rune layer stays thin, testable logic stays out of it, and the UI could be replaced. Rune is pinned to a specific `dev` revision and upgraded deliberately.

## Consequences

- Our `Cargo.toml` has to repeat Rune's `[patch]` entries for its vendored winit, wgpu-hal and parley.
- Builds are heavy; keep debug info at line tables only.
