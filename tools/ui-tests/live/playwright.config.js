// The browser suite against a live agent (C# or Rust), not the web demo. PHONEBOX_AGENT_CMD is the agent's command line
// (a relative program path is taken from the repository root); each spec file starts its own agent on free ports with
// a scratch HOME (fixtures.js, agent.js). From tools/ui-tests:
//   PHONEBOX_AGENT_CMD=agent-rs/target/release/PhoneBoxAgent PHONEBOX_AGENT_LABEL=rust npx playwright test -c live
// PHONEBOX_KEEP=1 keeps every agent's folder (config, stdout.log); a failed test keeps its agent's folder anyway.
// On the test server: agent-rs/scripts/remote-ui-live.sh (in the Playwright container); in CI: rust.yml's ui-live job.
const { defineConfig } = require('@playwright/test');
const path = require('path');

if (!process.env.PHONEBOX_AGENT_CMD || !process.env.PHONEBOX_AGENT_CMD.trim()) {
  throw new Error('PHONEBOX_AGENT_CMD is not set: the live suite needs the command line of the agent to test, e.g.\n'
    + '  PHONEBOX_AGENT_CMD=agent-rs/target/release/PhoneBoxAgent npx playwright test -c live');
}

const root = path.resolve(__dirname, '..', '..', '..');
const label = (process.env.PHONEBOX_AGENT_LABEL || 'agent').replace(/[^\w.-]/g, '_');

module.exports = defineConfig({
  testDir: __dirname,
  testMatch: '*.spec.js',
  timeout: 60_000,
  expect: { timeout: 10_000 },
  retries: 0,
  workers: 3,
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  outputDir: path.join(__dirname, '..', 'test-results', `live-${label}`),
  reporter: [['list'], ['html', { open: 'never', outputFolder: path.join(root, 'ui-test-report', `live-${label}`) }]],
  use: {
    viewport: { width: 1280, height: 860 },
    locale: 'en-US',
    trace: 'retain-on-failure',
  },
});
