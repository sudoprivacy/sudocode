// Test-only access to the production controller. Never exported by the package.
export { openCodexMailboxWake, readCodexMailboxSnapshot, mailboxSubmissionTextBudget,
  CODEX_MAILBOX_DELIVERY_PROVENANCE } from '../src/codexMailboxWake';
export { openCodexMailboxOwner } from '../src/codexMailboxOwner';
export { openScodeMailbox } from '../src/scodeMailbox';
export { upgradedMailboxProfile } from '../src/codexBindings';
