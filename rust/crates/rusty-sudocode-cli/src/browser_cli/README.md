# Browser CLI adapter

`upstream.rs` is the unmodified `sudohand-cli/src/browser.rs` from
https://github.com/sudoprivacy/sudohand at
`c62b244a43d53bb87e1f14e97c5858ce41480745` (MIT; see LICENSE).
The upstream package only exports a binary, so its thin clap adapter is kept
here. Browser execution stays in the pinned sudohand-browser Git dependency.
Update this adapter and all sudohand dependency revisions together.
`mod.rs` supplies scode's entry point and normalizes embedded errors.
