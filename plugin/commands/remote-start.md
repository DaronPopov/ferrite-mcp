---
description: Activate remote access for this machine — brings up Tailscale, starts tmux, triggers Claude boot, and sends an email when the session is ready to join.
argument-hint: [status|setup]
---

# Remote Activate

Boot full remote access for this machine. Execute every step automatically — fix blockers inline without stopping to ask. The boot daemon sends an email with the Claude session URL once it finishes loading (~2 min).

---

## Step 0 — Sudoers check

Use `mcp__ferrite__exec` to verify passwordless sudo for tailscale:

```
sudo -n tailscale status 2>&1
```

- Works: continue silently.
- Fails with password prompt: install the rule:
  ```
  echo "daron ALL=(ALL) NOPASSWD: /usr/bin/tailscale" | sudo tee /etc/sudoers.d/ferrite-tailscale && sudo chmod 440 /etc/sudoers.d/ferrite-tailscale
  ```
  If that also fails, note `WARN: sudoers not configured — run once manually` and continue.

---

## Step 1 — Tailscale up

Use `mcp__ferrite__tailscale_status` to check current state.

- **Running**: note the IP, continue.
- **Not running**: use `mcp__ferrite__exec` to run:
  ```
  sudo -n tailscale up --accept-routes
  ```
  Wait 3 seconds, then call `mcp__ferrite__tailscale_status` again to confirm the IP.
  If still down, note `WARN: Tailscale failed — SSH unavailable` and continue.

---

## Step 2 — tmux session

Use `mcp__ferrite__tmux_ctl` with `op: "list"`.

- **Sessions exist**: note session names, continue.
- **No sessions**: create one — `op: "new"`, `session: "main"`, `cmd: "bash"`. Confirm with another `list`.

---

## Step 3 — Boot daemon

Use `mcp__ferrite__exec` to check the autostart systemd unit:

```
systemctl --user status ferrite-session 2>&1 | head -5
```

- **Active (running)**: daemon is managing Claude + will send email — note "daemon already running", skip.
- **Inactive / stopped**: start it: `systemctl --user start ferrite-session` — note "daemon started".
- **Not found**: spawn directly:
  ```
  nohup ~/.local/bin/ferrite-autostart > ~/.local/share/ferrite/autostart.log 2>&1 &
  ```
  Note "daemon spawned directly".

The daemon flow: Tailscale up → Claude boots in tmux → captures session URL → sends Gmail + ntfy with SSH + tmux + session link.

---

## Step 4 — Read saved session URL

Use `mcp__ferrite__read_file` on `~/.local/share/ferrite/remote-session-url.txt`.
Include URL if present; otherwise note `pending — email coming when Claude boots`.

---

## Step 5 — Notify immediately

Use `mcp__ferrite__notify`:

```
title: "Remote access activating"
message: "SSH: ssh daron@<ip>  |  tmux attach -t main  |  Claude URL incoming via email"
phone: true
desktop: true
tags: ["rocket"]
priority: "high"
```

---

## Step 6 — Summary

```
REMOTE ACCESS — <hostname>
Tailscale IP : <ip or "DOWN">
SSH          : ssh daron@<ip>
Attach tmux  : tmux attach -t main
Session URL  : <url or "pending — email on the way">
Boot daemon  : <already running | started | spawned directly>
```

No prose. Append `WARN: <issue>` lines for anything that failed.

---

If `$ARGUMENTS` is `setup`, also report:
- `ls -la ~/.local/bin/ferrite-autostart`
- `systemctl --user status ferrite-session`
- `grep GMAIL_USER ~/.config/ferrite/gmail.conf 2>/dev/null`

As a `SETUP STATUS:` section after the main block.
