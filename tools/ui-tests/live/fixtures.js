// Playwright fixtures of the live suite. Each spec file sets its own `agentOptions` (test.use at its top), and a
// different worker option means a different worker, so every file gets an agent of its own. baseURL points at
// that agent; before each test its settings go back to the fixture (the startup Wi-Fi port and keys, so it never needs
// a restart) and the shortcut's uploaded icon is removed. A dirty settings page may reload: beforeunload is accepted.
const base = require('@playwright/test');
const { startAgent } = require('./agent');
const { SHORTCUT_ID } = require('./fixture');

const { expect } = base;

const test = base.test.extend({
  agentOptions: [{ name: 'live', wifi: true }, { scope: 'worker', option: true }],

  agent: [async ({ agentOptions }, use) => {
    const agent = await startAgent(agentOptions);
    try {
      await use(agent);
    } finally {
      await agent.stop();
    }
  }, { scope: 'worker', timeout: 90_000 }],

  baseURL: async ({ agent }, use) => { await use(agent.url); },

  context: async ({ context }, use) => {
    context.on('page', (page) => page.on('dialog', (dialog) =>
      (dialog.type() === 'beforeunload' ? dialog.accept() : dialog.dismiss()).catch(() => {})));
    await use(context);
  },

  resetAgent: [async ({ agent, request }, use, testInfo) => {
    // no X-Config-Version: a script's save never conflicts
    const put = await request.put('/api/settings', { data: agent.config });
    if (put.status() !== 200) throw new Error(`resetting the settings failed: ${put.status()} ${await put.text()}`);
    const del = await request.delete(`/api/settings/shortcut-icon?id=${SHORTCUT_ID}`);
    if (del.status() !== 200) throw new Error(`removing the uploaded icon failed: ${del.status()}`);
    await use();
    if (testInfo.status !== testInfo.expectedStatus) {
      agent.failed = true; // keeps its folder
      await testInfo.attach(`agent output (${agent.label})`, { body: agent.log(), contentType: 'text/plain' });
      await testInfo.attach('agent folder', { body: agent.dir, contentType: 'text/plain' });
    }
  }, { auto: true }],
});

/** A deep copy of the agent's fixture config, to change for an outside save. */
const fixtureCopy = (agent) => structuredClone(agent.config);

/** GET /api/settings: the config and its X-Config-Version. */
async function readSettings(request) {
  const res = await request.get('/api/settings');
  expect(res.status()).toBe(200);
  return { version: res.headers()['x-config-version'], cfg: await res.json() };
}

/**
 * Opens the settings page at a section (`#general`, ...) and waits until it has loaded: the version shows once every
 * editor has been drawn. Returns the X-Config-Version the page loaded.
 */
async function openSettings(page, agent, hash) {
  // on the page already, a goto that only changes the hash would not load it again
  if (page.url().startsWith(agent.url)) await page.goto('about:blank');
  const loaded = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/settings' && r.request().method() === 'GET');
  await page.goto(`${agent.url}${hash}`);
  const version = (await loaded).headers()['x-config-version'];
  await expect(page.locator('#ver')).toHaveText(`v${agent.info.version}`);
  return version;
}

/** Empties the toast, so the next toHaveText cannot match an earlier message. */
const clearToast = (page) => page.locator('#toast').evaluate((el) => { el.textContent = ''; });

/** Uncaught errors of the page (a broken script against this agent's answers), for an expect(...).toEqual([]). */
function watchErrors(page) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  return errors;
}

module.exports = { test, expect, fixtureCopy, readSettings, openSettings, clearToast, watchErrors };
