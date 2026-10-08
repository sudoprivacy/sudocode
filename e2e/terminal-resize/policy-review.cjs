// Executed only from the base checkout, before any candidate code is built.
const fs = require('node:fs');
const path = require('node:path');

module.exports = async function review({ github, context, core, root, output }) {
  const { owner, repo } = context.repo;
  const policyPath = 'e2e/terminal-resize/performance-policy.json';
  let policy = JSON.parse(fs.readFileSync(path.join(root, policyPath), 'utf8'));
  const pr = context.payload.pull_request;
  const head = pr?.head.sha || context.sha;
  let isNeeded = true;
  const provenance = { base: pr?.base.sha || context.sha, head, approved_by: [], changes: [] };
  if (pr) {
    const current = await github.rest.pulls.get({ owner, repo, pull_number: pr.number });
    if (current.data.head.sha !== head) throw new Error('PR advanced; run the current commit');
    const files = await github.paginate(github.rest.pulls.listFiles, {
      owner, repo, pull_number: pr.number, per_page: 100,
    });
    if (files.length !== current.data.changed_files) throw new Error('incomplete PR file list');
    const affectsPerformance = name => name.startsWith('rust/')
      || name.startsWith('e2e/terminal-resize/') || name === '.github/workflows/render-performance.yml';
    isNeeded = files.some(file => affectsPerformance(file.filename)
      || (file.previous_filename && affectsPerformance(file.previous_filename)));
    const protectedPath = name => name === '.github/workflows/render-performance.yml'
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
        if (['admin', 'maintain', 'write'].includes(permission.data.permission))
          provenance.approved_by.push(login);
      }
      if (!provenance.approved_by.length)
        throw new Error('Rendering benchmark policy/harness changed. A maintainer must approve this exact commit, then rerun the gate.');
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
  fs.writeFileSync(path.join(output, 'approval.json'), JSON.stringify(provenance, null, 2));
  core.setOutput('baseline', policy.baseline_commit);
  core.setOutput('head', head);
  core.setOutput('is_needed', String(isNeeded));
};
