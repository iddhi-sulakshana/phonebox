// The settings every live agent starts with (and that each test resets to): no phone, no update checks, no approvals,
// Wi-Fi on its own free port with the parity fixture certificate (so both agents show the same fingerprint), two pages
// with a note each (no network), and one shortcut that has an id and no program icon of its own.
const crypto = require('crypto');

const NOTE_TEXT = 'Fixture note text';
const SECOND_NOTE_TEXT = 'Second page note';
const SHORTCUT_ID = 'live-art';

// the agents' own key format (Config.cs NewToken): 32 random bytes, base64url without padding
const newKey = () => crypto.randomBytes(32).toString('base64url');

/** A fresh fixture config for an agent whose Wi-Fi port is `lanPort`; `wifi: false` leaves Wi-Fi off. */
function liveConfig({ lanPort, wifi = true }) {
  return {
    deviceSerial: '__ui_live_no_device__',
    updates: { auto: false, repo: 'iddhi-sulakshana/phonebox' },
    claude: { phoneApprovals: false },
    night: { enabled: false, start: '00:30', end: '07:30', brightness: 0.02 },
    rules: [],
    display: { language: 'en', theme: 'default' },
    lan: { enabled: wifi, port: lanPort, token: newKey(), webhookToken: newKey() },
    pages: [
      {
        id: 'live-notes', title: 'Notes', icon: '📝', enabled: true,
        widgets: [{ id: 'live-note', type: 'note', x: 0, y: 0, w: 6, h: 3, options: { text: NOTE_TEXT, color: '#fbbf24' } }],
      },
      {
        id: 'live-second', title: 'Second', icon: '🗒️', enabled: true,
        widgets: [{ id: 'live-note-2', type: 'note', x: 0, y: 0, w: 6, h: 3, options: { text: SECOND_NOTE_TEXT, color: '#60a5fa' } }],
      },
    ],
    shortcuts: [{ id: SHORTCUT_ID, label: 'Art', icon: '🎨', color: '#0ea5e9', cmd: 'builtin:lock' }],
  };
}

module.exports = { liveConfig, NOTE_TEXT, SECOND_NOTE_TEXT, SHORTCUT_ID };
