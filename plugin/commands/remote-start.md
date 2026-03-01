---
description: Report full remote access status for this machine — Tailscale IP, active tmux sessions, SSH command, and any saved Claude session URL. Run this to get everything needed to reach the machine from a phone or remote terminal.
argument-hint: [status|setup]
---

# Remote Control

Provide a complete remote access report for this machine. Execute all steps automatically — fix any blocking issues before reporting. Do NOT stop and ask the user for help; resolve each issue inline.

---

## Step 0 — Self-setup (always run first)

Use `mcp__ferrite__exec` to check whether passwordless sudo for tailscale is already configured:

```
sudo -n tailscale status 2>&1
```

- If the command fails with "sudo: a password is required" or similar, automatically install the sudoers rule:
  ```
  echo "daron ALL=(ALL) NOPASSWD: /usr/bin/tailscale" | sudo tee /etc/sudoers.d/ferrite-tailscale && sudo chmod 440 /etc/sudoers.d/ferrite-tailscale
  ```
  If that also requires a password, note "sudoers not yet configured — run once manually" and continue.
- If the rule is already present, continue silently.

---

## Step 1 — Tailscale (auto-start)

Use `mcp__ferrite__tailscale_status` to check status.

- If running: note IP and hostname, continue.
- If NOT running: use `mcp__ferrite__exec` to run `sudo -n tailscale up --accept-routes --reset`, then call `mcp__ferrite__tailscale_status` again.
  - If it still fails, note the error but continue to other steps.

---

## Step 2 — tmux (auto-create)

Use `mcp__ferrite__tmux_ctl` with `op: "list"` to list active sessions.

- If there ARE sessions: note their names.
- If there are NO sessions (error or empty): automatically create one with `op: "new"`, `session: "main"`, `cmd: "bash"`. Then list again to confirm.

---

## Step 3 — Saved session URL

Use `mcp__ferrite__read_file` to read `~/.local/share/ferrite/remote-session-url.txt`.
Include the URL if present, otherwise note "none".

---

## Step 4 — Summary output

Print this exact block with real values filled in:

```
REMOTE ACCESS — <hostname>
Tailscale IP : <ip or "DOWN — sudo tailscale up needed">
SSH          : ssh daron@<ip>
Attach tmux  : tmux attach -t main
Session URL  : <url or "none">
```

No prose before or after the block. If anything could not be auto-fixed, append a single-line `WARN:` note per issue below the block.

If $ARGUMENTS is "setup", additionally use `mcp__ferrite__exec` to check:
- `ls -la ~/.local/bin/ferrite-autostart` — installed or not
- `systemctl --user status ferrite-autostart` — active/inactive/missing
Report both in a `SETUP STATUS:` section after the main block.
