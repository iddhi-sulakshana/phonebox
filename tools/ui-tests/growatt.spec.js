// The Growatt widget: the inverter's live power flow (solar, battery, load, grid) with a Live / Stale / Cloud badge,
// today's energy, and the inverter's settings as actions (AC output source, charge source, max charge current,
// relogin). The demo answers /api/growatt* inside the page; window.PB_DEMO_GROWATT overrides parts of its view.
const { test, expect } = require('@playwright/test');

async function open(page, options = {}, size = {}, override = null) {
  if (override) await page.addInitScript((o) => { window.PB_DEMO_GROWATT = o; }, override);
  await page.goto('/panel/');
  await page.evaluate(({ options, size }) => {
    localStorage.clear();
    localStorage.setItem('pb-demo-config', JSON.stringify({
      night: { enabled: false }, shortcuts: [], display: { idleReturnSec: 0 },
      pages: [{ id: 'solar', title: 'Solar', enabled: true,
        widgets: [{ id: 'g1', type: 'growatt', x: 0, y: 0, w: size.w || 12, h: size.h || 6, options }] }],
    }));
  }, { options, size });
  await page.reload();
  await expect(page.locator('.w-growatt')).toHaveCount(1);
  // every POST the widget sends, for the assertions
  await page.evaluate(() => {
    window.posts = [];
    const f = window.fetch;
    window.fetch = (u, o) => { if (o?.method === 'POST') window.posts.push(String(u).replace(/^.*\/api/, '/api')); return f(u, o); };
  });
}

const act = (page, name) => page.locator('.gw-act', { hasText: name });

test('the inverter card shows the flow, today and the actions', async ({ page }) => {
  const errors = []; page.on('pageerror', (e) => errors.push(String(e)));
  await open(page, { control: true });
  const w = page.locator('.w-growatt');
  await expect(w.locator('.w-title')).toHaveText('Solar');
  await expect(w.locator('.gw-badge')).toHaveText('Live');
  const tiles = w.locator('.gw-tile');
  await expect(tiles).toHaveCount(4);
  await expect(tiles.nth(0).locator('.gw-lbl')).toHaveText('Solar');
  await expect(tiles.nth(0).locator('.gw-big')).toHaveText('1264 W');
  await expect(tiles.nth(0).locator('.gw-sub')).toHaveText('PV1 604 W · PV2 660 W');
  await expect(tiles.nth(1).locator('.gw-big')).toHaveText('75%');
  await expect(tiles.nth(1).locator('.gw-sub')).toHaveText('Charging 43 W · 54.4 V');
  await expect(tiles.nth(2).locator('.gw-big')).toHaveText('1270 W');
  await expect(tiles.nth(2).locator('.gw-sub')).toHaveText('23% of the inverter');
  await expect(tiles.nth(3).locator('.gw-big')).toHaveText('0 W');
  await expect(tiles.nth(3).locator('.gw-sub')).toHaveText('232 V · 50.1 Hz');
  const today = w.locator('.gw-kwh');
  await expect(today).toHaveCount(4);
  await expect(today.nth(0)).toContainText('Solar today');
  await expect(today.nth(0).locator('b')).toHaveText('5.2 kWh');
  await expect(today.nth(0).locator('small')).toHaveText('1235 kWh in all');
  await expect(today.nth(2)).toContainText('From the grid');
  await expect(act(page, 'AC output')).toContainText('Solar First');
  await expect(act(page, 'Charging')).toContainText('Solar First');
  await expect(act(page, 'Max charge')).toContainText('30 A');
  await expect(act(page, 'Relogin')).toHaveCount(1);
  await expect(w.locator('.gw-err')).toHaveCount(0);
  expect(errors).toEqual([]);
});

test('choosing an AC output source posts it', async ({ page }) => {
  await open(page, { control: true });
  await act(page, 'AC output').click();
  const sheet = page.locator('.gw-sheet');
  await expect(sheet).toBeVisible();
  await expect(sheet.locator('.gw-choice')).toHaveCount(4);
  await expect(sheet.locator('.gw-choice.cur')).toHaveText('Solar First');
  await sheet.locator('.gw-choice', { hasText: 'Utility First' }).click();
  await expect(sheet).toHaveCount(0);
  await expect(act(page, 'AC output')).toContainText('Utility First');
  expect(await page.evaluate(() => window.posts)).toEqual(['/api/growatt/settings/ac-output-source?value=2']);
});

test('max charge offers the presets and a value of your own', async ({ page }) => {
  await open(page, { control: true });
  await act(page, 'Max charge').click();
  const sheet = page.locator('.gw-sheet');
  await expect(sheet.locator('.gw-choice')).toHaveText(['1 A', '5 A', '10 A', '15 A', '20 A', '25 A', '30 A', '100 A']);
  await expect(sheet.locator('.gw-choice.cur')).toHaveText('30 A');
  await sheet.locator('input[type=number]').fill('42');
  await sheet.locator('.gw-set').click();
  await expect(act(page, 'Max charge')).toContainText('42 A');
  // out of range: refused on the phone, nothing sent
  await act(page, 'Max charge').click();
  await page.locator('.gw-sheet input[type=number]').fill('140');
  await page.locator('.gw-sheet .gw-set').click();
  await expect(page.locator('.gw-sheet')).toBeVisible();
  expect(await page.evaluate(() => window.posts)).toEqual(['/api/growatt/settings/max-charge-current?value=42']);
});

test('relogin asks the server to reconnect to Growatt', async ({ page }) => {
  await open(page, { control: true });
  await act(page, 'Relogin').click();
  await expect(page.locator('.toast').last()).toContainText('reconnected');
  expect(await page.evaluate(() => window.posts)).toEqual(['/api/growatt/relogin']);
});

test('without control there are no actions', async ({ page }) => {
  await open(page, { control: false });
  await expect(page.locator('.gw-tile')).toHaveCount(4);
  await expect(page.locator('.gw-act')).toHaveCount(0);
});

test('an error keeps the readings', async ({ page }) => {
  await open(page, { control: true }, {}, { error: 'The Growatt server did not answer' });
  await expect(page.locator('.gw-err')).toHaveText('The Growatt server did not answer');
  await expect(page.locator('.gw-tile').nth(0).locator('.gw-big')).toHaveText('1264 W');
});

test('not set up says where to set it up', async ({ page }) => {
  await open(page, { control: true }, {}, { configured: false, error: 'Set up Growatt in Settings › General', status: null, totals: null });
  await expect(page.locator('.w-growatt .empty')).toHaveText('Set up Growatt in Settings › General');
  await expect(page.locator('.gw-tile')).toHaveCount(0);
});

test('a small widget stays legible', async ({ page }) => {
  await open(page, { control: true }, { w: 4, h: 3 });
  const w = page.locator('.w-growatt');
  await expect(w.locator('.gw-tile').first()).toBeVisible();
  const spill = await w.evaluate((e) => e.scrollWidth > e.clientWidth + 1 || [...e.querySelectorAll('.gw-tile')].some((t) => {
    const a = t.getBoundingClientRect(), b = e.getBoundingClientRect();
    return a.right > b.right + 1 || a.bottom > b.bottom + 1;
  }));
  expect(spill).toBe(false);
});

// Settings › General: the Growatt and TCL cards' Test buttons send the typed (unsaved) values
test('the settings page tests the typed values', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 860 });
  await page.goto('/settings/#general');
  await page.evaluate(() => localStorage.clear());
  await page.reload();
  await page.evaluate(() => {
    window.tests = [];
    const f = window.fetch;
    window.fetch = (u, o) => {
      if (/-test$/.test(String(u))) window.tests.push([String(u).replace(/^.*\/api/, '/api'), JSON.parse(o.body)]);
      return f(u, o);
    };
  });
  await page.locator('input[data-bind="growatt.url"]').fill('http://growatt.lan');
  await page.locator('input[data-bind="growatt.password"]').fill('sun');
  await expect(page.locator('input[data-bind="growatt.password"]')).toHaveAttribute('type', 'password');
  await page.locator('#growattTest').click();
  await expect(page.locator('#growattState')).toHaveText('Connected');
  await page.locator('input[data-bind="tcl.username"]').fill('me@example.com');
  await page.locator('input[data-bind="tcl.password"]').fill('pw');
  await expect(page.locator('input[data-bind="tcl.password"]')).toHaveAttribute('type', 'password');
  await page.locator('#tclTest').click();
  await expect(page.locator('#tclState')).toHaveText('Connected · 3 units');
  expect(await page.evaluate(() => window.tests)).toEqual([
    ['/api/growatt-test', { url: 'http://growatt.lan', password: 'sun' }],
    ['/api/tcl-test', { username: 'me@example.com', password: 'pw' }],
  ]);
});

// final review: a re-render every few seconds rebuilt the open sheet and wiped what was typed
test('the max charge field keeps what is typed while the readings refresh', async ({ page }) => {
  await open(page, { control: true });
  await act(page, 'Max charge').click();
  const input = page.locator('.gw-sheet input[type=number]');
  await input.fill('42');
  await input.evaluate((e) => { e.dataset.mark = 'typed'; });
  await page.waitForTimeout(6500); // past the readings' 3 s polls and the 5 s age refresh
  await expect(page.locator('.gw-sheet input[data-mark="typed"]')).toHaveValue('42');
  await page.locator('.gw-sheet .gw-set').click();
  await expect(act(page, 'Max charge')).toContainText('42 A');
});

// final review: the settings were read once; a failed first read left the labels at "—" for good
test('the settings labels are read again after a failed read', async ({ page }) => {
  await page.clock.install({ time: new Date('2026-11-23T14:00:00') });
  await page.addInitScript(() => {
    window.PB_DEMO_GROWATT_SETTINGS = { error: 'The Growatt server did not answer', acOutputSource: null, chargeSource: null, maxChargeCurrent: null };
  });
  await open(page, { control: true });
  await expect(act(page, 'AC output')).toContainText('—');
  await page.evaluate(() => { delete window.PB_DEMO_GROWATT_SETTINGS; });
  await page.clock.runFor(70_000);
  await expect(act(page, 'AC output')).toContainText('Solar First');
  await expect(act(page, 'Max charge')).toContainText('30 A');
});
