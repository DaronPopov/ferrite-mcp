---
description: Report full remote access status for this machine — Tailscale IP, active tmux sessions, SSH command, and any saved Claude session URL. Run this to get everything needed to reach the machine from a phone or remote terminal.
argument-hint: [status|setup]
---

# Remote Control

Provide a complete remote access report for this machine. Do the following steps in order, then present a single clean summary.

## Step 1 — Tailscale

Use `mcp__ferrite__tailscale_status` to get the machine's Tailscale IP and whether it is reachable. Note the IP and hostname.

If Tailscale is not running, output a warning and skip the SSH section.

## Step 2 — tmux sessions

Use `mcp__ferrite__tmux_ctl` with `op: "list"` to list active sessions. Include session names and status.

## Step 3 — Saved session URL

Read `~/.local/share/ferrite/remote-session-url.txt` using `mcp__ferrite__read_file`. If the file exists and is non-empty, include the URL in the summary. If missing, note "no saved session URL".

## Step 4 — Summary output

Print a compact block with everything needed to connect:

```
REMOTE ACCESS — <hostname>
Tailscale IP : <ip>
SSH          : ssh daron@<ip>
Attach tmux  : tmux attach -t <session>  (or list: tmux ls)
Session URL  : <url or "none">
```

Keep the output short. No prose — just the connection facts. If $ARGUMENTS is "setup", also check whether `ferrite-autostart` is installed (`~/.local/bin/ferrite-autostart`) and whether a systemd unit for it exists, and report their status.
