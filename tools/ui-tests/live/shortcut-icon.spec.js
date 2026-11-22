// A shortcut's own icon against a live agent: uploading an SVG or a PNG through the page, the phone's upload-only
// request and /api/config seeing it, removing it, the agent's 400 for a wrong type or a broken file, and the version an
// upload hands back (the same tab saves without a conflict; a tab loaded before it gets 409).
const { test, expect, readSettings, openSettings, clearToast } = require('./fixtures');
const { SHORTCUT_ID } = require('./fixture');

test.use({ agentOptions: { name: 'shortcut-icon', wifi: true } });

const SVG = { name: 'art.svg', mimeType: 'image/svg+xml',
  buffer: Buffer.from('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><rect width="16" height="16" fill="#0ea5e9"/></svg>') };
// a real 1x1 PNG, so the page's preview <img> loads it
const PNG = { name: 'art.png', mimeType: 'image/png',
  buffer: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=', 'base64') };
const UPLOAD_ONLY = `/api/shortcuts/icon?id=${SHORTCUT_ID}&upload=true`;

const editor = (page) => page.locator('#editor');
const preview = (page) => editor(page).locator('img.custom-icon-preview');

async function openShortcut(page, agent) {
  await openSettings(page, agent, '#shortcuts');
  await expect(page.locator('#deck .key.sel')).toContainText('Art');
  await expect(editor(page).getByRole('button', { name: 'Upload…' })).toBeVisible();
}

/** Picks a file through the editor's Upload… button, as a person does. */
async function upload(page, file) {
  await clearToast(page);
  const chooser = page.waitForEvent('filechooser');
  await editor(page).getByRole('button', { name: 'Upload…' }).click();
  await (await chooser).setFiles(file);
}

const iconUpload = async (request) =>
  (await (await request.get('/api/config')).json()).config.shortcuts.find((s) => s.id === SHORTCUT_ID).iconUpload;

test('an SVG icon uploads, shows, reaches the phone and can be removed', async ({ page, agent, request }) => {
  await openShortcut(page, agent);
  await expect(preview(page)).toBeHidden(); // nothing uploaded: the icon request is a 404
  expect((await request.get(UPLOAD_ONLY)).status()).toBe(404);

  await upload(page, SVG);
  await expect(page.locator('#toast')).toHaveText('Icon uploaded.');
  await expect(page.locator('#toast')).not.toHaveClass(/\berr\b/);
  await expect(preview(page)).toBeVisible();
  const served = await request.get(UPLOAD_ONLY);
  expect(served.status()).toBe(200);
  expect(served.headers()['content-type']).toMatch(/^image\/svg\+xml/);
  expect(await served.body()).toEqual(SVG.buffer);
  expect(await iconUpload(request)).toBe(true);

  await clearToast(page);
  await editor(page).getByRole('button', { name: 'Remove', exact: true }).click();
  await expect(page.locator('#toast')).toHaveText('Custom icon removed.');
  await expect(preview(page)).toBeHidden();
  expect((await request.get(UPLOAD_ONLY)).status()).toBe(404);
  expect(await iconUpload(request)).toBeFalsy();
});

test('a PNG icon uploads too', async ({ page, agent, request }) => {
  await openShortcut(page, agent);
  await upload(page, PNG);
  await expect(page.locator('#toast')).toHaveText('Icon uploaded.');
  await expect(preview(page)).toBeVisible();
  const served = await request.get(UPLOAD_ONLY);
  expect(served.status()).toBe(200);
  expect(served.headers()['content-type']).toMatch(/^image\/png/);
  expect(await served.body()).toEqual(PNG.buffer);
});

test('a file of another type or a broken SVG is refused with the agent\'s message', async ({ page, agent, request }) => {
  await openShortcut(page, agent);
  await upload(page, { name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('not an icon') });
  await expect(page.locator('#toast')).toHaveText('Only PNG, WEBP or SVG icons are supported');
  await expect(page.locator('#toast')).toHaveClass(/\berr\b/);
  await upload(page, { name: 'broken.svg', mimeType: 'image/svg+xml', buffer: Buffer.from('<svg xmlns="http://www.w3.org/2000/svg"><rect></svg>') });
  await expect(page.locator('#toast')).toHaveText('That does not look like a valid SVG file');
  await expect(page.locator('#toast')).toHaveClass(/\berr\b/);
  await expect(preview(page)).toBeHidden();
  expect((await request.get(UPLOAD_ONLY)).status()).toBe(404);
});

test('after an upload the same tab saves without a conflict and a tab loaded before it gets 409',
  async ({ page, context, agent, request }) => {
    const older = await context.newPage();
    await openSettings(older, agent, '#general');

    await openShortcut(page, agent);
    const before = (await readSettings(request)).version;
    await upload(page, SVG);
    await expect(page.locator('#toast')).toHaveText('Icon uploaded.');
    expect((await readSettings(request)).version, 'an upload moves the config version').not.toBe(before);

    // the page adopted the version the upload answered: its own save goes through
    await editor(page).locator('input[type="text"]').first().fill('Art edited');
    await clearToast(page);
    await page.locator('#save').click();
    await expect(page.locator('#toast')).toHaveText('Saved — the phone updates within a few seconds');
    expect((await readSettings(request)).cfg.shortcuts[0].label).toBe('Art edited');

    // the other tab still holds the version from before the upload
    await older.locator('[data-bind="display.theme"]').selectOption('graphite');
    await clearToast(older);
    await older.locator('#save').click();
    await expect(older.locator('#toast')).toHaveText(/^The settings were changed in another tab or window/);
    await expect(older.locator('#toast')).toHaveClass(/\berr\b/);
    expect((await readSettings(request)).cfg.display.theme).toBe('default');
  });
