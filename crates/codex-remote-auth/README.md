# codex-remote-auth

Email-only gate for a mobile Codex remote session UI.

Security layers implemented:
- Single-use signed magic links (`/auth/request` -> `/auth/redeem?token=...`)
- Second factor email OTP (6-digit code)
- HttpOnly + SameSite=Strict session cookie
- Session TTL + revoke-all endpoint (`/admin/revoke-all`)
- No public login page (`/app` requires authenticated session)

## Config

Create `~/.config/ferrite/codex_remote_auth.toml` from `config.example.toml`.

## Run

```bash
cargo run -p codex-remote-auth -- --config ~/.config/ferrite/codex_remote_auth.toml
```
