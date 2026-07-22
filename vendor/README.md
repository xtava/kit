# Vendored source and licenses

Kit embeds narrowly selected crates from two pinned upstream source roots for `kit console`.
Provenance and initial tree identities live outside those roots. Kit maintains its approved safety
changes directly in the vendored source so each checkout has one complete implementation.

| Source | Provenance | License coverage | Local maintenance |
| --- | --- | --- | --- |
| WezTerm | [`wezterm.upstream`](./wezterm.upstream) | The upstream root [`LICENSE.md`](./wezterm/LICENSE.md), [`licenses/README.md`](./wezterm/licenses/README.md), and all 26 inventoried license/notice paths are retained intact. | Directly maintained under `vendor/wezterm` in the four Console safety groups. |
| varbincode 0.1.0 | [`varbincode.upstream`](./varbincode.upstream) | The crate's [`LICENSE.md`](./varbincode/LICENSE.md) is retained intact. | Directly maintained under `vendor/varbincode` as part of the codec/decode safety group. |

WezTerm's four nested gitlinks are recorded in `wezterm.upstream` and intentionally remain
uninitialized: the headless Console dependency graph does not consume the GUI/font roots they
reference. A future need for those sources requires an explicit provenance and license decision.
Do not run a root-level recursive submodule update: the gitlinks belong to the nested upstream tree,
and Kit intentionally provides no root `.gitmodules` entries for them.

Do not create a second patch or scratch owner for these changes. Update the vendored source,
provenance record, lockfiles, focused tests, and clean-checkout verification together.
