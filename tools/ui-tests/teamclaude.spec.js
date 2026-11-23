// The AI quotas widget's TeamClaude card: the day's tokens as TeamClaude's two line panels (input with the cache, and
// output, each on its own scale with its busiest hour marked) over the weekly forecast (solid past, dashed projection,
// the dry line); beside them the top five users of the day and of all time; an error keeps the numbers; with paging
// the card has a page of its own.
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
      hourly: Array.from({ length: 24 }, (_, i) => (i + 1) * 3_030_000),
      hourlyInput: Array.from({ length: 24 }, (_, i) => (i === 9 ? 90_000_000 : (i + 1) * 3_000_000)),
      hourlyOutput: Array.from({ length: 24 }, (_, i) => (i + 1) * 30_000),
      bucketMs: HOUR,
      end: END,
      users: [user('alice', 900_000_000), user('bob', 300_000_000), user('carol', 200_000_000), user('dave', 120_000_000), user('erin', 90_000_000)],
      others: { count: 2, tokens: 40_000_000 },
      totalUsers: [user('bob', 9_000_000_000), user('alice', 8_000_000_000), user('frank', 3_000_000_000), user('carol', 2_000_000_000), user('erin', 1_000_000_000)],
      totalOthers: { count: 4, tokens: 500_000_000 },
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
      pages: [{ id: 'q', title: 'Quotas', widgets: [{ id: 'q1', type: 'quotas', x: 0, y: 0, w: opts.w || 12, h: opts.h || 6, options: opts }] }],
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
  // the graphs one under another: the tokens over the weekly pool, both in the left column
  const tokensBox = await card.locator('.q-tc-tokens').boundingBox();
  const poolBox = await card.locator('.q-tc-fc').boundingBox();
  expect(tokensBox.y + tokensBox.height).toBeLessThanOrEqual(poolBox.y + 1);
  expect(Math.abs(tokensBox.x - poolBox.x)).toBeLessThan(2);
  // TeamClaude's two token panels: input (cache included) and output, each a line over its area with its busiest hour
  const panels = card.locator('.q-tc-panel');
  await expect(panels).toHaveCount(2);
  await expect(panels.nth(0).locator('.q-tc-phead')).toHaveText('Input, cache included');
  await expect(panels.nth(1).locator('.q-tc-phead')).toHaveText('Output');
  await expect(card.locator('.q-tc-panel path.line')).toHaveCount(2);
  await expect(card.locator('.q-tc-panel path.area')).toHaveCount(2);
  await expect(panels.nth(0).locator('.q-tc-peak')).toHaveText('90.0M');
  await expect(panels.nth(1).locator('.q-tc-peak')).toHaveText('720.0k');
  await expect(card.locator('.q-tc-ticks span')).toHaveCount(4);
  await expect(card.locator('.q-tc-chart path.past')).toHaveCount(1);
  await expect(card.locator('.q-tc-chart path.proj')).toHaveCount(1);
  await expect(card.locator('.q-tc-chart line.thr')).toHaveCount(1);
  expect(await card.locator('.q-tc-chart path.proj').evaluate((e) => getComputedStyle(e).strokeDasharray)).not.toBe('none');
  await expect(card.locator('.q-tc-fc .q-tc-sub')).toHaveText('39% used · 0.1% unused at refill');
  const lists = card.locator('.q-tc-list');
  await expect(lists).toHaveCount(2);
  await expect(lists.nth(0).locator('.q-tc-lbl')).toHaveText('Top users · 24 h');
  await expect(lists.nth(1).locator('.q-tc-lbl')).toHaveText('Top users · total');
  await expect(lists.nth(1).locator('.q-tc-user')).toHaveCount(6);
  await expect(lists.nth(1).locator('.q-tc-user').nth(0).locator('.q-tc-uname')).toHaveText('bob');
  await expect(lists.nth(1).locator('.q-tc-user').nth(0).locator('.q-tc-uval')).toHaveText('9.0B');
  await expect(lists.nth(1).locator('.q-tc-user').nth(5)).toContainText('+ 4 others');
  // every bar starts at the same place and has the same track, in both lists
  const tracks = await card.locator('.q-tc-utrack').evaluateAll((els) => els.map((e) => { const r = e.getBoundingClientRect(); return [r.left, r.width]; }));
  expect(tracks.length).toBe(12);
  for (const [left, w] of tracks) {
    expect(Math.abs(left - tracks[0][0])).toBeLessThan(1);
    expect(Math.abs(w - tracks[0][1])).toBeLessThan(1);
  }
  const rows = lists.nth(0).locator('.q-tc-user');
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
  await expect(card.locator('.q-tc-list').nth(0).locator('.q-tc-user')).toHaveCount(6);
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

// review: a 4×3 widget is under the panel's 200 px rule that hides the footers; the TeamClaude error must stay
test('a small widget still shows the error', async ({ page }) => {
  await show(page, { providers: ['teamclaude'], w: 4, h: 3 }, sample({ error: 'TeamClaude refused the key (HTTP 401)' }));
  const card = page.locator('.w-quotas .q-team');
  await expect(card.locator('.q-err')).toBeVisible();
  await expect(card.locator('.q-tc-big')).toBeVisible();
  const clipped = await card.locator('.q-err').evaluate((e) => {
    const c = e.closest('.q-card').getBoundingClientRect();
    const r = e.getBoundingClientRect();
    return r.bottom > c.bottom + 1 || r.height < 8;
  });
  expect(clipped).toBe(false);
});
