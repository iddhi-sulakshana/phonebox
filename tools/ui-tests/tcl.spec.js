// The TCL widget: one card per air conditioner (inside and outside temperature; power, target temperature, mode, fan
// and generator when the widget allows control), an all-units bar, and offline units without controls. The demo
// answers /api/tcl* inside the page; window.PB_DEMO_TCL overrides parts of its view.
const { test, expect } = require('@playwright/test');

async function open(page, options = {}, size = {}, override = null) {
  if (override) await page.addInitScript((o) => { window.PB_DEMO_TCL = o; }, override);
  await page.goto('/panel/');
  await page.evaluate(({ options, size }) => {
    localStorage.clear();
    localStorage.setItem('pb-demo-config', JSON.stringify({
      night: { enabled: false }, shortcuts: [], display: { idleReturnSec: 0 },
      pages: [{ id: 'ac', title: 'AC', enabled: true,
        widgets: [{ id: 't1', type: 'tcl', x: 0, y: 0, w: size.w || 12, h: size.h || 6, options }] }],
    }));
  }, { options, size });
  await page.reload();
  await expect(page.locator('.w-tcl')).toHaveCount(1);
  await page.evaluate(() => {
    window.posts = [];
    const f = window.fetch;
    window.fetch = (u, o) => { if (o?.method === 'POST') window.posts.push(String(u).replace(/^.*\/api/, '/api')); return f(u, o); };
  });
}

const unit = (page, i) => page.locator('.tc-unit').nth(i);

test('the units show their state and offline ones have no controls', async ({ page }) => {
  const errors = []; page.on('pageerror', (e) => errors.push(String(e)));
  await open(page, { control: true });
  const w = page.locator('.w-tcl');
  await expect(w.locator('.w-title')).toHaveText('Air conditioners');
  await expect(w.locator('.tc-count')).toHaveText('1 of 2 on');
  await expect(page.locator('.tc-unit')).toHaveCount(3);
  const first = unit(page, 0);
  await expect(first.locator('.tc-name')).toHaveText('Shop front');
  await expect(first).toHaveClass(/\bon\b/);
  await expect(first.locator('.tc-in')).toHaveText('29.5°');
  await expect(first.locator('.tc-out')).toHaveText('Out 34°');
  await expect(first.locator('.tc-target')).toHaveText('26°');
  await expect(first.locator('.tc-chip.cur')).toHaveText(['Cool', 'Low', 'Off']);
  const offline = unit(page, 2);
  await expect(offline).toHaveClass(/offline/);
  await expect(offline).toContainText('Offline');
  await expect(offline.locator('button')).toHaveCount(0);
  expect(errors).toEqual([]);
});

test('the power button switches a unit', async ({ page }) => {
  await open(page, { control: true });
  const back = unit(page, 1);
  await expect(back).not.toHaveClass(/\bon\b/);
  await back.locator('.tc-pwr').click();
  await expect(back).toHaveClass(/\bon\b/);
  expect(await page.evaluate(() => window.posts)).toEqual(['/api/tcl/ac2/power?value=on']);
});

test('quick temperature taps send one change', async ({ page }) => {
  await open(page, { control: true });
  const first = unit(page, 0);
  await first.locator('.tc-plus').click();
  await first.locator('.tc-plus').click();
  await expect(first.locator('.tc-target')).toHaveText('28°');
  await expect.poll(() => page.evaluate(() => window.posts)).toEqual(['/api/tcl/ac1/temp?value=28']);
  await page.waitForTimeout(800);
  expect(await page.evaluate(() => window.posts)).toEqual(['/api/tcl/ac1/temp?value=28']);
  await expect(first.locator('.tc-target')).toHaveText('28°');
});

test('the all units bar sets the generator', async ({ page }) => {
  await open(page, { control: true });
  const gen = page.locator('.tc-all .tc-group', { hasText: 'Generator' });
  await gen.locator('.tc-btn', { hasText: '2' }).click();
  expect(await page.evaluate(() => window.posts)).toEqual(['/api/tcl/all/generator?value=2']);
  await expect(gen.locator('.tc-btn.cur')).toHaveText('2');
  await expect(unit(page, 0).locator('.tc-gens .tc-chip.cur')).toHaveText('2');
  await expect(unit(page, 1).locator('.tc-gens .tc-chip.cur')).toHaveText('2');
});

test('without control there are no buttons', async ({ page }) => {
  await open(page, { control: false });
  await expect(page.locator('.tc-unit')).toHaveCount(3);
  await expect(page.locator('.w-tcl button')).toHaveCount(0);
  await expect(unit(page, 0).locator('.tc-target')).toHaveText('26°');
});

test('an error keeps the units', async ({ page }) => {
  await open(page, { control: true }, {}, { error: 'TCL did not answer' });
  await expect(page.locator('.tc-err')).toHaveText('TCL did not answer');
  await expect(page.locator('.tc-unit')).toHaveCount(3);
});

test('a small widget stays legible', async ({ page }) => {
  await open(page, { control: true }, { w: 4, h: 3 });
  const w = page.locator('.w-tcl');
  await expect(w.locator('.tc-unit').first()).toBeVisible();
  expect(await w.evaluate((e) => e.scrollWidth > e.clientWidth + 1)).toBe(false);
  const card = await w.locator('.tc-unit').first().evaluate((c) => {
    const a = c.getBoundingClientRect(), b = c.closest('.w').getBoundingClientRect();
    return a.left >= b.left - 1 && a.right <= b.right + 1;
  });
  expect(card).toBe(true);
});
