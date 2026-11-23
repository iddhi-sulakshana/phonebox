import { API, PANEL_HEADERS, api, en, h, haptic, t, toast } from '../main.js';
import { W } from '../registry.js';
import { every } from './helpers.js';

// The shop inverter through the owner's Growatt server (the agent holds the sign-in): the live power flow, today's
// energy and, when the widget allows it, the inverter's settings and a relogin to Growatt's cloud.
const AC_OUTPUT = ['SBU Priority', 'Solar First', 'Utility First', 'SUB Priority'];
const CHARGE = ['Solar First', 'Solar and Utility', 'Only Solar'];
const PRESETS = [1, 5, 10, 15, 20, 25, 30, 100];
const LIVE_MS = 30_000; // a GroBro reading older than this is stale, as on the Growatt dashboard

const watts = (n) => n == null ? '—' : Math.abs(n) >= 10_000 ? `${(n / 1000).toFixed(1)} kW` : `${Math.round(n)} W`;
const kwh = (n) => n == null ? '—' : `${n >= 100 ? Math.round(n) : n.toFixed(1)} kWh`;
const fixed = (n, d, unit) => n == null ? null : `${n.toFixed(d)} ${unit}`;
const join = (...parts) => parts.filter(Boolean).join(' · ');

function ago(ms) {
  const s = Math.max(0, Math.round(ms / 1000));
  return s < 60 ? `${s} s ago` : s < 3600 ? `${Math.round(s / 60)} min ago` : `${Math.round(s / 3600)} h ago`;
}

W.growatt = {
  needs: ['tick'],
  create(box, { o }) {
    let data = null, settings = null, settingsAsked = false, busy = '', sheet = null, lastKey = '';
    const title = o.title === undefined ? en('Solar') : o.title;
    const head = h('div', 'gw-head', ...(title ? [h('div', 'w-title', title)] : []), h('span', 'gw-badge'), h('span', 'gw-age'));
    const body = h('div', 'gw-body');
    box.append(head, body);

    const load = async () => {
      try { data = await api('/growatt', 'GET', 20_000); } catch (_) { return false; }
      if (data?.control && !settingsAsked) { settingsAsked = true; loadSettings(); }
    };
    const loadSettings = async () => {
      try { settings = await api('/growatt/settings', 'GET', 60_000); } catch (_) {}
      render(true);
    };
    const post = async (path) => {
      try {
        const r = await fetch(`${API}/growatt/${path}`, { method: 'POST', cache: 'no-store', headers: PANEL_HEADERS });
        const j = await r.json().catch(() => ({}));
        if (!r.ok) { toast(j.error || 'Growatt did not take it', 'err'); return null; }
        return j;
      } catch (_) { toast(t('panel.pcNoAnswer'), 'err'); return null; }
    };
    const setSetting = async (name, key, value, label) => {
      haptic();
      sheet = null; busy = key; render(true);
      const ok = await post(`settings/${name}?value=${value}`);
      if (ok) { settings = { ...(settings || {}), [key]: value }; toast(`${label} set`, 'ok'); }
      busy = ''; render(true);
    };
    const relogin = async () => {
      haptic();
      busy = 'relogin'; render(true);
      const j = await post('relogin');
      if (j) toast(j.connected ? 'Growatt cloud reconnected' : 'Growatt cloud is not connected', j.connected ? 'ok' : 'err');
      busy = ''; render(true);
      load().then(() => render(true));
    };

    const choiceSheet = (list, current, pick) => h('div', 'gw-choices',
      ...list.map(([value, label]) => {
        const b = h('button', 'gw-choice' + (value === current ? ' cur' : ''), label);
        b.addEventListener('click', (e) => { e.stopPropagation(); pick(value, label); });
        return b;
      }));
    const openSheet = (kind) => { haptic(); sheet = kind; render(true); };
    const sheetEl = () => {
      const s = settings || {};
      let titleText, content;
      if (sheet === 'ac') {
        titleText = 'AC output source';
        content = choiceSheet(AC_OUTPUT.map((l, i) => [i, l]), s.acOutputSource,
          (v, l) => setSetting('ac-output-source', 'acOutputSource', v, `AC output: ${l}`));
      } else if (sheet === 'charge') {
        titleText = 'Charge source';
        content = choiceSheet(CHARGE.map((l, i) => [i, l]), s.chargeSource,
          (v, l) => setSetting('charge-source', 'chargeSource', v, `Charging: ${l}`));
      } else {
        titleText = 'Max charge current';
        const pick = (v) => setSetting('max-charge-current', 'maxChargeCurrent', v, `Max charge: ${v} A`);
        const input = h('input', 'gw-num');
        input.type = 'number'; input.min = '0'; input.max = '100'; input.step = '1'; input.inputMode = 'numeric';
        input.placeholder = '0–100';
        input.addEventListener('click', (e) => e.stopPropagation());
        const set = h('button', 'gw-set', 'Set');
        set.addEventListener('click', (e) => {
          e.stopPropagation();
          const v = Number(input.value);
          if (input.value.trim() === '' || !Number.isInteger(v) || v < 0 || v > 100) { toast('Choose a whole number from 0 to 100 A', 'err'); return; }
          pick(v);
        });
        content = h('div', null, choiceSheet(PRESETS.map((v) => [v, `${v} A`]), s.maxChargeCurrent, pick),
          h('div', 'gw-own', input, h('span', 'gw-unit', 'A'), set));
      }
      const close = h('button', 'gw-close', '✕');
      close.addEventListener('click', (e) => { e.stopPropagation(); sheet = null; render(true); });
      const panel = h('div', 'gw-panel', h('div', 'gw-ptitle', titleText, close), content);
      panel.addEventListener('click', (e) => e.stopPropagation());
      const el = h('div', 'gw-sheet', panel);
      el.addEventListener('click', () => { sheet = null; render(true); });
      return el;
    };

    const tile = (cls, label, big, sub, extra) => h('div', 'gw-tile ' + cls, h('div', 'gw-lbl', label), h('div', 'gw-big', big), h('div', 'gw-sub', sub || ''), extra || null);

    const render = (force = false) => {
      const st = data?.status;
      const key = JSON.stringify([data, settings, busy, sheet, Math.floor(Date.now() / 5000)]);
      if (!force && key === lastKey) return;
      lastKey = key;
      const badge = head.querySelector('.gw-badge'), age = head.querySelector('.gw-age');
      if (!data) { body.replaceChildren(h('div', 'empty', t('w.loading'))); badge.textContent = age.textContent = ''; return; }
      if (!data.configured || (!st && data.error)) {
        body.replaceChildren(h('div', 'empty', data.error || 'Growatt is not set up'));
        badge.textContent = age.textContent = ''; badge.className = 'gw-badge';
        return;
      }
      const old = st?.updatedAt ? Date.now() - st.updatedAt : Infinity;
      const live = st?.source === 'grobro' ? (old < LIVE_MS ? 'live' : 'stale') : st ? 'cloud' : '';
      badge.className = 'gw-badge ' + live;
      badge.textContent = { live: 'Live', stale: 'Stale', cloud: 'Cloud' }[live] || '';
      age.textContent = st?.updatedAt ? ago(old) : '';

      const batW = st?.batteryW;
      const batState = batW == null ? null : batW < 0 ? `Charging ${watts(-batW)}` : batW > 0 ? `Discharging ${watts(batW)}` : 'Idle';
      const pct = st?.batteryPct;
      const bar = h('div', 'gw-bar', h('i'));
      bar.firstChild.style.width = `${Math.max(0, Math.min(100, pct ?? 0))}%`;
      const tiles = h('div', 'gw-flow',
        tile('solar', 'Solar', watts(st?.pvW), join(st?.pv1W != null && `PV1 ${watts(st.pv1W)}`, st?.pv2W != null && `PV2 ${watts(st.pv2W)}`)),
        tile('battery' + (batW < 0 ? ' charging' : batW > 0 ? ' discharging' : ''), 'Battery', pct == null ? '—' : `${Math.round(pct)}%`,
          join(batState, fixed(st?.batteryV, 1, 'V')), bar),
        tile('load', 'Load', watts(st?.loadW), st?.loadPct == null ? '' : `${Math.round(st.loadPct)}% of the inverter`),
        tile('grid', 'Grid', watts(st?.gridW), join(st?.gridV != null && `${Math.round(st.gridV)} V`, fixed(st?.gridHz, 1, 'Hz'))));
      const tt = data.totals;
      const day = (label, e) => h('div', 'gw-kwh', h('span', null, label), h('b', null, kwh(e?.today)), h('small', null, e?.total == null ? '' : `${kwh(e.total)} in all`));
      const today = h('div', 'gw-today', day('Solar today', tt?.solar), day('Battery out', tt?.discharged), day('From the grid', tt?.imported), day('Used', tt?.load));
      const kids = [tiles, today];
      if (data.control) {
        const s = settings || {};
        const value = (list, v) => v == null ? (settings?.error ? '—' : '…') : (list[v] ?? String(v));
        const button = (label, val, onClick, k) => {
          const b = h('button', 'gw-act' + (busy === k ? ' busy' : ''), h('span', 'gw-act-l', label), val == null ? null : h('b', null, val));
          b.addEventListener('click', onClick);
          return b;
        };
        kids.push(h('div', 'gw-acts',
          button('AC output', value(AC_OUTPUT, s.acOutputSource), () => openSheet('ac'), 'acOutputSource'),
          button('Charging', value(CHARGE, s.chargeSource), () => openSheet('charge'), 'chargeSource'),
          button('Max charge', s.maxChargeCurrent == null ? value([], null) : `${s.maxChargeCurrent} A`, () => openSheet('max'), 'maxChargeCurrent'),
          button('↻ Relogin', null, relogin, 'relogin')));
      }
      if (data.error) kids.push(h('div', 'gw-err', data.error));
      if (sheet) kids.push(sheetEl());
      body.replaceChildren(...kids);
    };
    const tick = every(3000, async () => { await load(); render(); });
    return { update: () => { tick(); render(); } };
  },
};
