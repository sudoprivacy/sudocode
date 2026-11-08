#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh
CARGO_TEST=(cargo_in_workspace test --manifest-path "$RUST_DIR/Cargo.toml" -q -p runtime --test mailbox_nexus_live)
daemon_logs() { tail -40 "$AUTHON_DATA_DIR/daemon.log" 2>/dev/null || true; }
echo "== [deterministic] two scode processes, one daemon =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" \
  NEXUS_A2A_TEST_RECEIVER_CREDENTIAL="$(authon_mint "team-lead")" \
  NEXUS_A2A_TEST_SENDER_CREDENTIAL="$(authon_mint "duet-sender")" \
  cargo_in_workspace test --manifest-path "$RUST_DIR/Cargo.toml" -q -p rusty-sudocode-cli \
  --test pty_agent_duet -- --nocapture

echo "== [deterministic] ACP close joins an acquired receiver before replying =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" \
  NEXUS_A2A_TEST_RECEIVER_CREDENTIAL="$(authon_mint "acp-close-receiver")" \
  cargo_in_workspace test --manifest-path "$RUST_DIR/Cargo.toml" -q -p rusty-sudocode-cli \
  --test acp_a2a_receive live_session_close_releases_an_acquired_receiver_seat_before_reply \
  -- --ignored --nocapture

echo "== [deterministic] a real receiver lease handoff preserves terminal drafts =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" \
  NEXUS_A2A_TEST_RECEIVER_CREDENTIAL="$(authon_mint "receiver-draft-probe")" \
  cargo_in_workspace test --manifest-path "$RUST_DIR/Cargo.toml" -q -p rusty-sudocode-cli \
  --test pty_node_record live_node_lease_loss_preserves_editable_draft \
  -- --ignored --exact --test-threads=1 --nocapture

echo "== [deterministic] direct ACP uses no model; pause and lost reply use real mTLS =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
  NEXUS_A2A_TEST_DIRECT_CREDENTIAL="$(authon_mint "direct-acp-probe")" \
  cargo_in_workspace test --manifest-path "$RUST_DIR/Cargo.toml" -q -p rusty-sudocode-cli \
  --test acp_integration live_direct_a2a_ -- --ignored --test-threads=1 --nocapture

# The path an ACP client drives with no model turn: `_sudocode/a2a/send` runs
# the session's own `send` tool. The node must stamp the session's
# authenticated identity on exactly one append, and a call cancelled after its
# append must answer "outcome unknown" without resending. The test skips
# without its credentials, so a skip here is a harness bug, not a pass.
echo "== [deterministic] an ACP client's direct send through the node =="
direct_out="$(NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" \
  NEXUS_A2A_TEST_DIRECT_SENDER_CREDENTIAL="$DIRECT_SENDER_BUNDLE" \
  NEXUS_A2A_TEST_DIRECT_RECEIVER_CREDENTIAL="$DIRECT_RECEIVER_BUNDLE" \
  cargo_in_workspace test --manifest-path "$RUST_DIR/Cargo.toml" -q -p rusty-sudocode-cli \
  --test acp_integration direct_a2a_through_a_node -- --nocapture 2>&1)" \
  || { printf '%s\n' "$direct_out"; daemon_logs; exit 1; }
printf '%s\n' "$direct_out"
if printf '%s' "$direct_out" | grep -q 'SKIP(nexus)'; then
  echo "!! the direct-send test skipped under the harness" >&2
  exit 1
fi

echo "== [deterministic] a silent connection errors, then recovers =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
  "${CARGO_TEST[@]}" live_a_silent_transport_errors_and_then_recovers -- --ignored --nocapture

echo "== [deterministic] an unminted session recovers its original node receiver =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
  NEXUS_A2A_TEST_PENDING_CREDENTIAL="$(authon_mint "pending-node-actor")" \
  cargo_in_workspace test --manifest-path "$RUST_DIR/Cargo.toml" -q -p rusty-sudocode-cli \
  --test acp_integration live_pending_node_recovers_send_and_the_original_receiver \
  -- --ignored --test-threads=1 --nocapture
