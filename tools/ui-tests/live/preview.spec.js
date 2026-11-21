// The layout editor's live preview: the agent's own /panel/ in a frame, calling the agent through /agent/api/, fed the
// unsaved settings by the page.
const { test, expect, readSettings, openSettings, watchErrors } = require('./fixtures');
const { NOTE_TEXT } = require('./fixture');

test.use({ agentOptions: { name: 'preview', wifi: true } });

const preview = (page) => page.frameLocator('#previewBox iframe');
const activeDot = (page) => preview(page).locator('#dots i')
  .evaluateAll((dots) => dots.findIndex((d) => d.classList.contains('on')));

async function openPages(page, agent) {
  await openSettings(page, agent, '#pages');
  await expect(page.locator('#pageTabs .ptab')).toHaveCount(2);
}

test('the live preview runs the agent\'s panel with the saved note', async ({ page, agent }) => {
  const errors = watchErrors(page);
  const panelCalls = [];
  page.on('response', (r) => { if (new URL(r.url()).pathname.startsWith('/agent/api/')) panelCalls.push(r.status()); });
  await openPages(page, agent);
  await page.locator('#stageSeg button[data-v="live"]').click();
  await expect(page.locator('#previewBox')).toBeVisible();
  await expect(page.locator('#previewBox iframe')).toHaveAttribute('src', '../panel/?preview=1');
  await expect(preview(page).locator('.note-t').first()).toHaveText(NOTE_TEXT);
  // the panel reaches the agent through /agent/api/..., which the local port answers as /api/...
  await expect.poll(() => panelCalls.filter((s) => s === 200).length).toBeGreaterThan(0);
  expect(errors).toEqual([]);
});

test('an unsaved edit shows in the preview and is not saved', async ({ page, agent, request }) => {
  await openPages(page, agent);
  await page.locator('#canvas [title="Note"]').click();
  const text = page.locator('#inspector textarea').first();
  await expect(text).toHaveValue(NOTE_TEXT);
  await text.fill('Edited in the live suite');
  await expect(page.locator('#save')).toBeEnabled();

  await page.locator('#stageSeg button[data-v="live"]').click();
  await expect(preview(page).locator('.note-t').first()).toHaveText('Edited in the live suite');
  // a second edit while the preview is showing reaches it too
  await text.fill('Edited again');
  await expect(preview(page).locator('.note-t').first()).toHaveText('Edited again');
  expect((await readSettings(request)).cfg.pages[0].widgets[0].options.text).toBe(NOTE_TEXT);
});

test('picking another page in the editor moves the preview there', async ({ page, agent }) => {
  await openPages(page, agent);
  await page.locator('#stageSeg button[data-v="live"]').click();
  await expect(preview(page).locator('.note-t').first()).toHaveText(NOTE_TEXT);
  await expect.poll(() => activeDot(page)).toBe(0);
  await page.locator('#pageTabs .ptab').nth(1).click();
  await expect.poll(() => activeDot(page)).toBe(1);
});

test('only the panel may be framed, and only by the settings page', async ({ request, agent }) => {
  const panel = await request.get('/panel/');
  expect(panel.status()).toBe(200);
  expect(panel.headers()['x-frame-options']).toBe('SAMEORIGIN');
  const settings = await request.get('/');
  expect(settings.status()).toBe(200);
  expect(settings.headers()['x-frame-options']).toBe('DENY');
  const bare = await request.get('/panel?preview=1', { maxRedirects: 0 });
  expect(bare.status()).toBe(302);
  expect(bare.headers().location).toBe('/panel/?preview=1');
  const info = await request.get('/agent/api/info');
  expect(info.status()).toBe(200);
  expect((await info.json()).version).toBe(agent.info.version);
});
