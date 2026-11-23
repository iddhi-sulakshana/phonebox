import { API, PANEL_HEADERS, api, en, h, haptic, t, toast } from '../main.js';
import { W } from '../registry.js';
import { every } from './helpers.js';

// The TCL air conditioners through TCL's cloud (the agent holds the account): one card per unit with its inside and
// outside temperature and, when the widget allows it, power, target temperature, mode, fan and generator, plus an
// all-units bar. A value just sent shows until the unit reports it (or 8 s pass): the cloud accepts at once, the unit
// takes a moment.
const MIN_T = 16, MAX_T = 31;
const PENDING_MS = 8000;
const DEBOUNCE_MS = 400;
const MODE = { cool: 1, fan: 3, dry: 2, heat: 4, auto: 0 };
const MODE_TARGET = { cool: 26, fan: 31, dry: 16, heat: 31 };
const deg = (n) => n == null ? '—' : `${Math.round(n * 10) / 10}°`;

W.tcl = {
  needs: ['tick'],
  create(box, { o }) {
    let data = null, lastKey = '';
    const pending = {}; // unit id -> { field: value }, until: ms
    const tempTimers = {};
    const title = o.title === undefined ? en('Air conditioners') : o.title;
    const head = h('div', 'tc-head', ...(title ? [h('div', 'w-title', title)] : []), h('span', 'tc-count'));
    const body = h('div', 'tc-body');
    box.append(head, body);

    const load = async () => { try { data = await api('/tcl', 'GET', 30_000); } catch (_) { return false; } };

    /** The unit as shown: its report, with values just sent over it until it reports them. */
    const shown = (u) => {
      const p = pending[u.id];
      if (!p) return u;
      if (Date.now() > p.until) { delete pending[u.id]; return u; }
      const out = { ...u };
      for (const [k, v] of Object.entries(p.values)) {
        if (u[k] === v) delete p.values[k]; else out[k] = v;
      }
      if (!Object.keys(p.values).length) delete pending[u.id];
      return out;
    };
    const expect = (ids, values) => {
      for (const id of ids) pending[id] = { values: { ...(pending[id]?.values || {}), ...values }, until: Date.now() + PENDING_MS };
    };
    /** What an action changes on a unit, for the pending values. */
    const effect = (action, value) => {
      if (action === 'power') return value === 'on' ? { power: true, target: 26 } : { power: false };
      if (action === 'mode') return { mode: MODE[value], ...(MODE_TARGET[value] != null ? { target: MODE_TARGET[value] } : {}) };
      if (action === 'temp') return { target: Number(value) };
      if (action === 'fan') return { fan: Number(value) };
      return { generator: Number(value) };
    };
    const send = async (unitId, action, value) => {
      const ids = unitId === 'all' ? (data?.units || []).filter((u) => u.online).map((u) => u.id) : [unitId];
      expect(ids, effect(action, value));
      render(true);
      try {
        const r = await fetch(`${API}/tcl/${encodeURIComponent(unitId)}/${action}?value=${encodeURIComponent(value)}`,
          { method: 'POST', cache: 'no-store', headers: PANEL_HEADERS });
        const j = await r.json().catch(() => ({}));
        if (!r.ok) { toast(j.error || 'TCL did not take it', 'err'); for (const id of ids) delete pending[id]; }
      } catch (_) { toast(t('panel.pcNoAnswer'), 'err'); for (const id of ids) delete pending[id]; }
      render(true);
      setTimeout(async () => { await load(); render(true); }, 1500);
    };
    const tap = (unitId, action, value) => (e) => { e.stopPropagation(); haptic(); send(unitId, action, String(value)); };
    const nudge = (u, step) => (e) => {
      e.stopPropagation(); haptic();
      const now = shown(u).target ?? 26;
      const next = Math.max(MIN_T, Math.min(MAX_T, Math.round(now) + step));
      expect([u.id], { target: next });
      render(true);
      clearTimeout(tempTimers[u.id]);
      tempTimers[u.id] = setTimeout(() => send(u.id, 'temp', String(next)), DEBOUNCE_MS);
    };

    const chips = (cls, list, current, onPick) => h('div', 'tc-chips ' + cls, ...list.map(([label, value, action, arg]) => {
      const b = h('button', 'tc-chip' + (value === current ? ' cur' : ''), label);
      if (onPick) b.addEventListener('click', onPick(action, arg));
      return b;
    }));
    const card = (raw) => {
      const u = shown(raw);
      if (!u.online) {
        return h('div', 'tc-unit offline', h('div', 'tc-top', h('span', 'tc-name', u.name), h('span', 'tc-dot')),
          h('div', 'tc-offline', 'Offline'));
      }
      const top = h('div', 'tc-top', h('span', 'tc-name', u.name), h('span', 'tc-dot'));
      const temps = h('div', 'tc-temps', h('div', 'tc-in', deg(u.inside)), h('div', 'tc-out', u.outside == null ? '' : `Out ${deg(u.outside)}`));
      const kids = [top, temps];
      if (data.control) {
        const pwr = h('button', 'tc-pwr' + (u.power ? ' on' : ''), '⏻');
        pwr.addEventListener('click', tap(u.id, 'power', u.power ? 'off' : 'on'));
        const minus = h('button', 'tc-minus', '−'), plus = h('button', 'tc-plus', '+');
        minus.addEventListener('click', nudge(raw, -1));
        plus.addEventListener('click', nudge(raw, +1));
        kids.push(h('div', 'tc-row', pwr, h('div', 'tc-temp', minus, h('span', 'tc-target', deg(u.target)), plus)));
        const pick = (action, arg) => tap(u.id, action, arg);
        kids.push(h('div', 'tc-opts',
          chips('tc-modes', [['Cool', 1, 'mode', 'cool'], ['Fan', 3, 'mode', 'fan']], u.mode, pick),
          chips('tc-fans', [['Low', 2, 'fan', 2], ['Max', 7, 'fan', 7]], u.fan, pick),
          chips('tc-gens', [['Off', 0, 'generator', 0], ['1', 1, 'generator', 1], ['2', 2, 'generator', 2], ['3', 3, 'generator', 3]], u.generator, pick)));
      } else {
        kids.push(h('div', 'tc-row', h('span', 'tc-state', u.power ? 'On' : 'Off'), h('div', 'tc-temp', h('span', 'tc-target', deg(u.target)))));
      }
      return h('div', 'tc-unit' + (u.power ? ' on' : ' off'), ...kids);
    };
    const allBar = (units) => {
      const online = units.filter((u) => u.online);
      const same = (k) => online.length && online.every((u) => u[k] === online[0][k]) ? online[0][k] : undefined;
      const group = (label, buttons, current) => h('div', 'tc-group', h('span', 'tc-glabel', label), ...buttons.map(([text, value, action, arg]) => {
        const b = h('button', 'tc-btn' + (value === current ? ' cur' : ''), text);
        b.addEventListener('click', tap('all', action, arg));
        return b;
      }));
      return h('div', 'tc-all',
        group('Power', [['Off', false, 'power', 'off'], ['On', true, 'power', 'on']], same('power')),
        group('Mode', [['Cool', 1, 'mode', 'cool'], ['Fan', 3, 'mode', 'fan'], ['Auto', 0, 'mode', 'auto']], same('mode')),
        group('Generator', [['Off', 0, 'generator', 0], ['1', 1, 'generator', 1], ['2', 2, 'generator', 2], ['3', 3, 'generator', 3]], same('generator')),
        group('Fan', [['Mid', 4, 'fan', 4], ['Max', 7, 'fan', 7]], same('fan')));
    };

    const render = (force = false) => {
      const key = JSON.stringify([data, pending]);
      if (!force && key === lastKey) return;
      lastKey = key;
      const count = head.querySelector('.tc-count');
      if (!data) { body.replaceChildren(h('div', 'empty', t('w.loading'))); count.textContent = ''; return; }
      const units = (data.units || []).map(shown);
      if (!data.configured || (!units.length && data.error)) {
        body.replaceChildren(h('div', 'empty', data.error || 'TCL is not set up'));
        count.textContent = '';
        return;
      }
      const online = units.filter((u) => u.online);
      count.textContent = `${online.filter((u) => u.power).length} of ${online.length} on`;
      const kids = [];
      if (data.control && online.length) kids.push(allBar(units));
      kids.push(units.length ? h('div', 'tc-units', ...(data.units || []).map(card)) : h('div', 'empty', 'No units in the TCL account'));
      if (data.error) kids.push(h('div', 'tc-err', data.error));
      body.replaceChildren(...kids);
    };
    const tick = every(5000, async () => { await load(); render(); });
    return { update: () => { tick(); render(); } };
  },
};
