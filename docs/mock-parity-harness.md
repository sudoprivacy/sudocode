# Mock parity harness

The mock parity harness exercises the `scode` CLI end-to-end against a
deterministic, Anthropic-compatible mock backend in a clean environment.
It is the measurement vehicle for the e2e coverage goal described in
[`../ROADMAP.html`](../ROADMAP.html) (Goal 1).

## Components

- `rust/crates/mock-anthropic-service/` — the deterministic
  `/v1/messages` mock service.
- `rust/crates/rusty-sudocode-cli/tests/mock_parity_harness.rs` —
  the end-to-end harness with isolated environment variables.
- `rust/scripts/run_mock_parity_harness.sh` — the wrapper script for
  local runs.
- `rust/mock_parity_scenarios.json` — the scenario manifest.

## Running the harness

```bash
cd rust/
./scripts/run_mock_parity_harness.sh
```

Behavioral diff against the scenario manifest:

```bash
cd rust/
python3 scripts/run_mock_parity_diff.py
```

## Running the mock service alone

For ad-hoc CLI runs against the mock:

```bash
cd rust/
cargo run -p mock-anthropic-service -- --bind 127.0.0.1:0
```

The server prints `MOCK_ANTHROPIC_BASE_URL=...` on startup. Point
`ANTHROPIC_BASE_URL` at that URL and use any non-empty
`ANTHROPIC_API_KEY` to drive `scode` through it.

## Scenario manifest

Scenario names, categories, and the parity dimensions they cover are
maintained in `rust/mock_parity_scenarios.json`. New scenarios extend
this manifest along with their corresponding `ScenarioCase` entries in
the harness source.
