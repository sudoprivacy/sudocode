# Windows compiler-cache tool delivery

The compiler-cache consumer still uses its existing pinned tool. This producer
does not switch the consumer or establish a lifecycle speedup. It prepares a
maintained artifact for the Windows response-file defect fixed by upstream
[sccache #3026](https://github.com/mozilla/sccache/pull/3026).

The source is the immutable upstream 0.19.0 commit. Its archive, minimal backport
and original/patched Rust modules are SHA-256 pinned in
`.github/compiler-cache/response-files.json`. The production hunks come from the
merged upstream fix; the older module import and test-helper interfaces are adapted.
The existing system/shared-test vector macros use conventional square brackets to
pass Rust 1.100 Clippy; their expansion and every test/assertion remain unchanged. All
changed files have complete original/patched SHA-256 pins. No Cargo
dependency or release-profile setting is changed. The upstream tool's tests are
dependency qualification, not a new SudoCode unit-test layer or product coverage.
The patch uses zero context to avoid whitespace-only patch context lines; both
the complete input and output module hashes must match before compilation.

A ready producer PR runs on Windows with the original formatter, Clippy and
upstream lib/bin/system tests, then the optimized tool build. All compilations
use one job, existing debug-free acceptance settings and the original release
profile. Compiler workers remain BelowNormal and test drivers remain Normal.
The build binds its full source-tree digest into the compiler flags and requires
fresh tool library and executable artifacts. Shared targets are never cleaned.

The real-process control first requires the original official tool to fail a
command over the Windows limit with error206. The new artifact must compile the
same class of command, record a cold miss and a subsequent hit, invalidate changed
environment and feature inputs, and run real linked consumers at Normal priority.
These controls establish tool behavior, not a population speedup. The original
tool is prepared by the existing API helper from an immutable accepted SudoCode
snapshot. Candidate builds receive no GitHub API credentials.

The `Compiler cache tool` workflow can also qualify accepted main source by manual
dispatch. Publication is a separate main-only dispatch with `mode=publish` and
`qualified_run_id`. A read-only job checks the original successful run, accepted
source ancestry, exact recipe input hashes, archive contents and real probe
results. A separate write job publishes those bytes without rebuilding them.
It refuses an existing release/tag instead of replacing assets and explicitly
keeps the compiler tool out of the product's Latest release selection. Producer source
and qualification identities remain original, even when a normal merge adds
unrelated accepted source.

Only after the public artifact exists can a separate normal consumer change pin
its exact archive/hash and qualify the real native/Docker cache fixture. Other
platforms retain their original official tools and safe fallbacks. This does not
change any product test assertions, deadlines, worker pools, native field
ownership, installed runtime or production service.
