# CI test scope and package cadence

Main added the ACP package cadence section while this branch updated the
adjacent hosted compiler paragraph. Keep both: package integration stays on
four PR hosts with exact merged-revision artifacts on main, and hosted native
compilers use the available CPUs while local compilation remains one worker.

The merge preserves all commits and authors from both branches. No production
Rust, assertion, deadline or distribution platform changes during adaptation.
