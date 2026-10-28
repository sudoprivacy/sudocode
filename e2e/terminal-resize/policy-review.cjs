// Approval is checked before candidate execution. Measurements use the PR base.
const fs = require('node:fs');
const path = require('node:path');

const missingApproval = 'Rendering benchmark policy/harness changed. A maintainer must approve this exact commit, then rerun the gate.';
const adminApprovalMarker = '<!-- scode-render-harness-admin-approval -->';

module.exports = async function review({ github, context, core, root, output }) {
  const { owner, repo } = context.repo;
  const policyPath = 'e2e/terminal-resize/performance-policy.json';
  const inputs = context.payload.inputs || {};
  const requested = context.eventName === 'workflow_dispatch' && inputs.pull_request?.trim();
  let pr = context.payload.pull_request;
  if (requested) {
    if (!/^[1-9][0-9]*$/.test(requested) || !Number.isSafeInteger(Number(requested)))
      throw new Error('pull_request must be a positive PR number');
    pr = (await github.rest.pulls.get({ owner, repo, pull_number: Number(requested) })).data;
    if (pr.state !== 'open' || pr.base.ref !== 'main'
      || pr.head.repo.full_name !== `${owner}/${repo}` || pr.head.sha !== context.sha)
      throw new Error('Dispatch the open main PR from its current same-repository head branch');
  }
  let policy = JSON.parse(fs.readFileSync(path.join(root, policyPath), 'utf8'));
  const head = pr?.head.sha || context.sha;
  const base = pr?.base.sha || context.sha;
  // A manual run still measures with the policy and harness from main.
  if (requested) {
    const content = await github.rest.repos.getContent({ owner, repo, path: policyPath, ref: base });
    if (content.data.type !== 'file') throw new Error('performance policy must remain a file');
    policy = JSON.parse(Buffer.from(content.data.content, 'base64').toString('utf8'));
  }
  let isNeeded = true;
  const provenance = { base, head, approved_by: [], review_ids: [], changes: [], approval_method: 'none' };
  if (pr) {
    const current = await github.rest.pulls.get({ owner, repo, pull_number: pr.number });
    if (current.data.head.sha !== head) throw new Error('PR advanced; run the current commit');
    if (current.data.base.sha !== base) throw new Error('PR base advanced; merge current main before measuring');
    const files = await github.paginate(github.rest.pulls.listFiles, {
      owner, repo, pull_number: pr.number, per_page: 100,
    });
    if (files.length !== current.data.changed_files) throw new Error('incomplete PR file list');
    const affectsPerformance = name => name.startsWith('rust/')
      || name.startsWith('e2e/terminal-resize/') || name === '.github/workflows/render-performance.yml'
      || name === '.github/workflows/render-publish.yml';
    isNeeded = files.some(file => affectsPerformance(file.filename)
      || (file.previous_filename && affectsPerformance(file.previous_filename)));
    const protectedPath = name => name === '.github/workflows/render-performance.yml'
      || name === '.github/workflows/render-publish.yml'
      || name.startsWith('e2e/terminal-resize/')
      || name.startsWith('rust/crates/rusty-sudocode-cli/tests/common/')
      || name === 'rust/crates/rusty-sudocode-cli/tests/pty_render_performance.rs'
      || name.startsWith('rust/crates/mock-anthropic-service/');
    provenance.changes = files.filter(file => protectedPath(file.filename)
      || (file.previous_filename && protectedPath(file.previous_filename))).map(file => file.filename);
    if (provenance.changes.length) {
      const reviews = await github.paginate(github.rest.pulls.listReviews, {
        owner, repo, pull_number: pr.number, per_page: 100,
      });
      const latest = new Map();
      for (const review of reviews) {
        if (review.state !== 'COMMENTED' && review.user?.type === 'User')
          latest.set(review.user.login, review);
      }
      for (const [login, review] of latest) {
        if (review.state !== 'APPROVED' || review.commit_id !== head || login === pr.user.login) continue;
        const permission = await github.rest.repos.getCollaboratorPermissionLevel({
          owner, repo, username: login,
        });
        if (['admin', 'maintain', 'write'].includes(permission.data.permission)) {
          provenance.approved_by.push(login);
          provenance.review_ids.push(review.id);
        }
      }
      if (provenance.approved_by.length) provenance.approval_method = 'maintainer_review';
      const policyChanged = files.some(file => file.filename === policyPath || file.previous_filename === policyPath);
      if (!provenance.approved_by.length && !policyChanged) {
        // GitHub forbids authors from submitting APPROVED reviews. A marked
        // COMMENTED review records an administrator's explicit approval instead.
        for (const review of [...reviews].reverse()) {
          if (review.state !== 'COMMENTED' || review.user?.type !== 'User'
            || review.user.login !== pr.user.login || review.commit_id !== head
            || !review.body?.trimStart().startsWith(adminApprovalMarker)) continue;
          const permission = await github.rest.repos.getCollaboratorPermissionLevel({
            owner, repo, username: review.user.login,
          });
          if (permission.data.permission !== 'admin') continue;
          provenance.approved_by.push(review.user.login);
          provenance.review_ids.push(review.id);
          provenance.approval_method = 'administrator_review';
          break;
        }
      }
      if (!provenance.approved_by.length)
        throw new Error(missingApproval);
      // An approved budget/baseline change is intentional. The running harness
      // still comes from the base; a PR cannot execute its replacement gate.
      if (files.some(file => file.filename === policyPath)) {
        const content = await github.rest.repos.getContent({
          owner: pr.head.repo.owner.login, repo: pr.head.repo.name, path: policyPath, ref: head,
        });
        if (content.data.type !== 'file') throw new Error('performance policy must remain a file');
        policy = JSON.parse(Buffer.from(content.data.content, 'base64').toString('utf8'));
      }
    }
  }
  if (!/^[0-9a-f]{40}$/.test(policy.baseline_commit)) throw new Error('baseline must be an immutable full commit SHA');
  fs.mkdirSync(output, { recursive: true });
  fs.writeFileSync(path.join(output, 'performance-policy.json'), JSON.stringify(policy, null, 2));
  if (provenance.approved_by.length) {
    core.info(`${provenance.approval_method} by ${provenance.approved_by.join(', ')} at ${head}`);
  }
  fs.writeFileSync(path.join(output, 'approval.json'), JSON.stringify(provenance, null, 2));
  core.setOutput('baseline', policy.baseline_commit);
  core.setOutput('head', head);
  core.setOutput('is_needed', String(isNeeded));
  core.setOutput('trusted', base);
  core.setOutput('repository', pr?.head.repo.full_name || `${owner}/${repo}`);
  core.setOutput('pull_number', pr?.number || '');
  return provenance;
};

module.exports.adminApprovalMarker = adminApprovalMarker;
