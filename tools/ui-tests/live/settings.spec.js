// Loading and saving the settings against a live agent: the saved value persists, Ctrl+S saves, the agent's own 400
// message shows, and a save over an outside change gets the agent's 409 and keeps the draft.
const { test, expect, fixtureCopy, readSettings, openSettings, clearToast, watchErrors } = require('./fixtures');

test.use({ agentOptions: { name: 'settings', wifi: true } });

const SAVED = 'Saved — the phone updates within a few seconds';
const CONFLICT = 'The settings were changed in another tab or window after this one loaded them. Reload the page to see '
  + 'them; the changes made here were not saved.';
const theme = (page) => page.locator('[data-bind="display.theme"]');

test('the page shows the agent\'s settings and a Save persists the change', async ({ page, agent, request }) => {
  const errors = watchErrors(page);
  const loadedVersion = await openSettings(page, agent, '#general');
  await expect(theme(page)).toHaveValue('default');
  await expect(page.locator('#save')).toBeDisabled();

  await theme(page).selectOption('graphite');
  await expect(page.locator('#save')).toBeEnabled();
  await page.locator('#save').click();
  await expect(page.locator('#toast')).toHaveText(SAVED);
  await expect(page.locator('#toast')).not.toHaveClass(/\berr\b/);
  await expect(page.locator('#save')).toBeDisabled();

  const after = await readSettings(request);
  expect(after.cfg.display.theme).toBe('graphite');
  expect(after.version).not.toBe(loadedVersion);
  await openSettings(page, agent, '#general');
  await expect(theme(page)).toHaveValue('graphite');
  expect(errors).toEqual([]);
});

test('Ctrl+S saves', async ({ page, agent, request }) => {
  await openSettings(page, agent, '#general');
  await theme(page).selectOption('warm');
  await page.keyboard.press('Control+s');
  await expect(page.locator('#toast')).toHaveText(SAVED);
  await expect(page.locator('#save')).toBeDisabled();
  expect((await readSettings(request)).cfg.display.theme).toBe('warm');
});

test('a save the agent refuses shows its message and keeps the draft', async ({ page, agent, request }) => {
  await openSettings(page, agent, '#general');
  const port = page.locator('[data-bind="lan.port"]');
  await expect(port).toHaveValue(String(agent.lanPort));
  await port.fill(String(agent.port)); // the Wi-Fi port may not be the local one: 400
  await clearToast(page);
  await page.locator('#save').click();
  await expect(page.locator('#toast')).toHaveText(`Wi-Fi port cannot be the same as the local port (${agent.port})`);
  await expect(page.locator('#toast')).toHaveClass(/\berr\b/);
  await expect(page.locator('#save')).toBeEnabled();
  await expect(port).toHaveValue(String(agent.port));
  expect((await readSettings(request)).cfg.lan.port).toBe(agent.lanPort);
});

test('a save over a change made elsewhere gets 409 and keeps the draft', async ({ page, agent, request }) => {
  const loadedVersion = await openSettings(page, agent, '#general');
  await theme(page).selectOption('graphite');

  // an outside save (a script, another tab); the version is the file's write time, so on a coarse clock a save in the
  // same tick as the page's load keeps the version: save again until it moved
  const outside = fixtureCopy(agent);
  outside.display.theme = 'amoled';
  let version = loadedVersion;
  for (let i = 0; i < 50 && version === loadedVersion; i++) {
    if (i) await new Promise((resolve) => setTimeout(resolve, 20));
    expect((await request.put('/api/settings', { data: outside })).status()).toBe(200);
    version = (await readSettings(request)).version;
  }
  expect(version, 'the outside save moved the config version').not.toBe(loadedVersion);

  await clearToast(page);
  await page.locator('#save').click();
  await expect(page.locator('#toast')).toHaveText(CONFLICT);
  await expect(page.locator('#toast')).toHaveClass(/\berr\b/);
  await expect(page.locator('#save')).toBeEnabled();
  await expect(theme(page)).toHaveValue('graphite');
  expect((await readSettings(request)).cfg.display.theme).toBe('amoled');

  // reloading the dirty page (beforeunload accepted) shows the outside change
  await openSettings(page, agent, '#general');
  await expect(theme(page)).toHaveValue('amoled');
  await expect(page.locator('#save')).toBeDisabled();
});
