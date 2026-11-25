# Persistent window, blocking CLI calls

An agent drives Akshually through a blocking CLI call (`akshually review ...`) that attaches to the Session for its Changeset. The call opens the app, or reuses the instance already running, where each Session is a tab. It returns only when the human hands over a Round or gives the Verdict, printing what it received; the first line of what it prints says which of the two it was, and the call exits 0 either way. The app outlives each call, so the human keeps context across Rounds and across parallel Sessions. That requires a single app instance and a local channel between short-lived CLI processes and it.

Agents launch Akshually; Akshually never launches agents, so it stays agnostic about which agent is used.

## Considered Options

- **The window lives inside a single blocking process**: simpler, but the window closes between Rounds, the reviewer loses context, and parallel Sessions would mean scattered windows.
- **An MCP server**: ties Akshually to agents that speak MCP. Any agent with a shell can use the CLI, and an MCP wrapper can be layered on top later.
- **`review` signals the outcome through its exit code** (a code each for a Round, Abandoned, Published, Closed): dropped because agent shells show any non-zero exit as a failed command, so every normal Round looked like an error to the human. Exit codes are left to real failures.
