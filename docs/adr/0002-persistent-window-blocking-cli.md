# Persistent window, blocking CLI calls

An agent drives akshually through a blocking CLI call (`akshually review ...`) that attaches to the Session for its Changeset. The call opens the app, or reuses the instance already running, where each Session is a tab. It returns only when the human hands over a Round or gives the Verdict, printing what it received and signalling which of the two it was through its exit code. The app outlives each call, so the human keeps context across Rounds and across parallel Sessions. That requires a single app instance and a local channel between short-lived CLI processes and it.

Agents launch akshually; akshually never launches agents, so it stays agnostic about which agent is used.

## Considered Options

- **The window lives inside a single blocking process**: simpler, but the window closes between Rounds, the reviewer loses context, and parallel Sessions would mean scattered windows.
- **An MCP server**: ties akshually to agents that speak MCP. Any agent with a shell can use the CLI, and an MCP wrapper can be layered on top later.
