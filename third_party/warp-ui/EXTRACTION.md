# Warp UI Extraction

Source: https://github.com/warpdotdev/warp
Commit: `3f0ac51`

Warp's README identifies the UI framework as the MIT-licensed `warpui` and
`warpui_core` crates. This directory vendors those crates plus the minimal
Warp-local support crates needed for `cargo check -p warpui --lib` to resolve:

- `warpui`
- `warpui_core`
- `markdown_parser`
- `sum_tree`
- `string-offset`
- `warp_util`
- `command`
- `asset_cache`
- `asset_macro`
- `virtual_fs`
- `settings_value`
- `settings_value_derive`

The root `Cargo.toml` in this directory has been narrowed to this extracted
workspace. The original Warp app and non-UI product crates are intentionally not
included.

License note: `warpui` and `warpui_core` declare `MIT`. Several support crates
inherit Warp's workspace `AGPL-3.0-only` license. Keep `LICENSE-MIT` and
`LICENSE-AGPL` with this vendored code.
