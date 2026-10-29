# Local acceptance migration

Install the managed hook once from a checkout containing main's acceptance
runner. The setting is repository-local and shared by this clone's worktrees:

```sh
python -m pip install psutil jsonschema
python .github/scripts/local_acceptance.py install --signer DEVICE_ID --key /absolute/path/to/private-key
```

The current Windows desktop is enrolled and installed. Other computers enroll
their own public Ed25519 device key through review of
`spec/local_acceptance_signers.json`; keep the private key on that computer.
No always-on Mac, remote login or local runner service is needed.

| Author host | Mandatory local native checks | Complementary hosted checks |
| --- | --- | --- |
| Windows | Windows | Linux and macOS |
| macOS | macOS | Linux and Windows |
| Linux | Linux | Windows and macOS |

The pre-push hook selects applicable checks, runs them, publishes signed Git
notes and then allows the branch push. Existing branches use the protected
runner without a cosmetic merge. If a check fails, retain the original logs;
the next attempt gets a fresh result directory. Do not amend a receipt, mark
an unrun profile passed, or use a mock provider result as live-model evidence.

Old PR heads that were pushed before hook installation need a real local
acceptance run and a new synchronization event. From that PR's clean worktree:

```sh
python .github/scripts/local_acceptance.py check --publish
```

Then push the next substantive commit normally. If the old branch predates the
runner files, the installed pre-push hook still loads protected-main commands.
A branch with no remaining code changes can trigger its first verified push
without rewriting published history:

```sh
git commit --allow-empty -m "ci: verify existing PR with local acceptance"
git push
```

That commit runs the same real applicable checks; it does not waive them.
Keep published commits and other contributors' work intact.

Full rendering/browser/viewport measurements use local Docker automatically.
Native cases remain in the workspace and complementary jobs; tagged release
artifact publishing retains its platform matrix. See CONTRIBUTING.md for
signature trust, coverage and the merge-group runtime tradeoff.
