# Viewport renderer integration

Integrate `ea9d5fe44ef788c9eec864ebacea21056bcfe83d` from #1115 into #1116,
preserving its commit and its enabled-before/after measurement support.

Resolve the Cargo manifest and lock records to iocraft
`5be9d43867dc7c1cf0da89330e6aa04a3cb94e77`. That revision combines #28's bounded
layout and measurement reuse with #29's immutable row reuse. Other dependency
records retain the original resolution.

Freeze the normal release binary before Cargo compiles test drivers, which can
otherwise rebuild the CLI with a different dependency feature graph. Streaming,
startup, viewport measurements and the Windows artifact use that frozen binary.
The before/after baseline uses an isolated target directory to avoid stale
same-version path-crate artifacts. Workloads, sample counts and budgets remain
the existing ones.

Goal 1 records the release-artifact and measurement boundaries. Feature coverage
is unchanged. The additional source comparison requires the explicit
`viewport-performance-comparison` label; the automatic on/off budget gate
remains in place. The broader CI workload reduction is a separate change.
