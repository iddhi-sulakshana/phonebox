// The AI quotas widget's TeamClaude card: the day's tokens with hourly bars, the weekly forecast (solid past, dashed
// projection, the dry line), the top five users and the rest; an error keeps the numbers; with paging the card has a
// page of its own.
const { test, expect } = require('@playwright/test');

const HOUR = 3_600_000;
const END = Date.now();

function sample(extra = {}) {
  const user = (name, tokens) => ({ name, tokens, share: tokens / 1_650_000_000 });
  return {
    providers: [
      { id: 'claude', name: 'Claude', plan: 'Max', limits: [{ label: 'Session', percent: 40, resetsAt: END + HOUR, windowSec: 18000 }], source: 'local' },
      { id: 'codex', name: 'Codex', plan: 'Plus', limits: [{ label: 'Weekly', percent: 10, resetsAt: END + 86_400_000, windowSec: 604800 }], source: 'local' },
    ],
    overview: null,
    teamclaude: {
      name: 'TeamClaude',
      host: 'tc.example.com',
      updatedAt: END - 30_000,
      error: null,
      tokens: { total: 1_650_000_000, input: 1_640_000_000, output: 10_000_000, cache: 1_500_000_000 },
      hourly: Array.from({ length: 24 }, (_, i) => (i + 1) * 3_000_000),
      bucketMs: HOUR,
      end: END,
      users: [user('alice', 900_000_000), user('bob', 300_000_000), user('carol', 200_000_000), user('dave', 120_000_000), user('erin', 90_000_000)],
      others: { count: 2, tokens: 40_000_000 },
      forecast: {
        verdict: 'on-pace', reason: null, confidence: 'high', pace: 0.767, threshold: 0.995, poolDryAt: null, unusedPct: 0.1,
        generatedAt: END,
        past: Array.from({ length: 96 }, (_, i) => [END - (95 - i) * 15 * 60_000, 0.2 + i * 0.002]),
        projected: Array.from({ length: 48 }, (_, i) => [END + i * HOUR * 3, 0.39 + i * 0.01]),
      },
      ...extra,
    },
  };
}

async function show(page, options, data) {
  await page.goto('/panel/');
  await page.evaluate((opts) => {
    localStorage.clear();
    localStorage.setItem('pb-demo-config', JSON.stringify({
      night: { enabled: false }, shortcuts: [], display: { idleReturnSec: 0 },
      pages: [{ id: 'q', title: 'Quotas', widgets: [{ id: 'q1', type: 'quotas', x: 0, y: 0, w: 12, h: 6, options: opts }] }],
    }));
  }, options);
  await page.reload();
  await expect(page.locator('.w-quotas')).toHaveCount(1);
  await page.evaluate(async (q) => {
    const { D, emit } = await import('/panel/main.js');
    D.quotas = q;
    emit('quotas');
  }, data);
}

test('the TeamClaude card shows the day, the forecast and the top users', async ({ page }) => {
  await show(page, { providers: ['teamclaude'] }, sample());
  const card = page.locator('.w-quotas .q-team');
  await expect(card).toHaveCount(1);
  await expect(card.locator('.q-name')).toHaveText('TeamClaude');
  await expect(card.locator('.q-tc-verdict')).toHaveText('On pace · 0.77×');
  await expect(card.locator('.q-upd')).toContainText('tc.example.com');
  await expect(card.locator('.q-tc-big')).toHaveText('1.6B');
  await expect(card.locator('.q-tc-tokens .q-tc-sub')).toHaveText('In 1.6B · Out 10.0M');
  await expect(card.locator('.q-tc-bars rect')).toHaveCount(24);
  await expect(card.locator('.q-tc-chart path.past')).toHaveCount(1);
  await expect(card.locator('.q-tc-chart path.proj')).toHaveCount(1);
  await expect(card.locator('.q-tc-chart line.thr')).toHaveCount(1);
  expect(await card.locator('.q-tc-chart path.proj').evaluate((e) => getComputedStyle(e).strokeDasharray)).not.toBe('none');
  await expect(card.locator('.q-tc-fc .q-tc-sub')).toHaveText('39% used · 0.1% unused at refill');
  const rows = card.locator('.q-tc-user');
  await expect(rows).toHaveCount(6);
  await expect(rows.nth(0).locator('.q-tc-uname')).toHaveText('alice');
  await expect(rows.nth(0).locator('.q-tc-uval')).toHaveText('900.0M');
  await expect(rows.nth(5)).toContainText('+ 2 others');
  // the busiest user's bar is full, the others in proportion
  const width = (i) => rows.nth(i).locator('.q-tc-ubar').evaluate((e) => e.getBoundingClientRect().width);
  const track = await rows.nth(0).locator('.q-tc-utrack').evaluate((e) => e.getBoundingClientRect().width);
  expect(await width(0)).toBeCloseTo(track, 0);
  expect(await width(1)).toBeCloseTo(track / 3, 0);
  // nothing spills out of the card
  const overflow = await card.evaluate((e) => [...e.querySelectorAll('.q-tc-sec')].some((s) => s.scrollWidth > s.clientWidth + 1));
  expect(overflow).toBe(false);
});

test('an error keeps the last numbers on the card', async ({ page }) => {
  await show(page, { providers: ['teamclaude'] }, sample({ error: 'TeamClaude refused the key (HTTP 401)' }));
  const card = page.locator('.w-quotas .q-team');
  await expect(card.locator('.q-err')).toHaveText('TeamClaude refused the key (HTTP 401)');
  await expect(card.locator('.q-tc-big')).toHaveText('1.6B');
  await expect(card.locator('.q-tc-user')).toHaveCount(6);
});

test('no users and no forecast say so', async ({ page }) => {
  await show(page, { providers: ['teamclaude'] }, sample({ users: [], others: null, forecast: null }));
  const card = page.locator('.w-quotas .q-team');
  await expect(card.locator('.q-tc-none')).toHaveText('No user traffic in the last day');
  await expect(card.locator('.q-tc-fc .q-tc-sub')).toHaveText('No forecast');
  await expect(card.locator('.q-tc-verdict')).toHaveCount(0);
});

test('with paging the TeamClaude card has a page of its own', async ({ page }) => {
  await show(page, { providers: ['claude', 'codex', 'teamclaude'], pageSize: 2, rotate: true, rotateInterval: 120 }, sample());
  const cards = page.locator('.w-quotas .q-card');
  await expect(cards).toHaveCount(1);
  await expect(cards.first()).toHaveClass(/q-team/);
  await page.locator('.w-quotas .q-grid').click();
  await expect(cards).toHaveCount(2);
  await expect(page.locator('.w-quotas .q-team')).toHaveCount(0);
});

test('without TeamClaude among the providers there is no card', async ({ page }) => {
  await show(page, { providers: ['claude', 'codex'] }, sample());
  await expect(page.locator('.w-quotas .q-card')).toHaveCount(2);
  await expect(page.locator('.w-quotas .q-team')).toHaveCount(0);
});
