// Pairing a phone over Wi-Fi against a live agent: the dialog's two QR codes decode to the pairing link and the app
// download that /api/pair/start answered (QR markup differs between the agents and is never compared), the link
// carries the certificate the Wi-Fi port really serves, closing the dialog cancels the code, a phone's claim over
// Wi-Fi closes it, and an agent with Wi-Fi off asks to turn it on.
const https = require('https');
const jsQR = require('jsqr');
const { test, expect, openSettings, clearToast, watchErrors } = require('./fixtures');
const { startAgent } = require('./agent');

test.use({ agentOptions: { name: 'pairing', wifi: true } });

const reEscape = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const codeOf = (link) => new URLSearchParams(new URL(link).hash.slice(1)).get('c');

async function startPairing(page) {
  const started = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/pair/start');
  await page.locator('#lanPair').click();
  const res = await started;
  expect(res.status()).toBe(200);
  return res.json();
}

/** The text of the QR code an <img> shows: drawn on a canvas in the page, decoded with jsQR. */
async function decodeQr(img) {
  const { size, pixels } = await img.evaluate(async (el) => {
    await el.decode();
    const side = 512;
    const canvas = document.createElement('canvas');
    canvas.width = side;
    canvas.height = side;
    const g = canvas.getContext('2d');
    g.fillStyle = '#fff';
    g.fillRect(0, 0, side, side);
    g.drawImage(el, 0, 0, side, side);
    const data = g.getImageData(0, 0, side, side).data;
    let bin = '';
    for (let i = 0; i < data.length; i += 0x8000) bin += String.fromCharCode.apply(null, data.subarray(i, i + 0x8000));
    return { size: side, pixels: btoa(bin) };
  });
  const found = jsQR(new Uint8ClampedArray(Buffer.from(pixels, 'base64')), size, size);
  return found ? found.data : null;
}

/** POST /api/pair/claim over the Wi-Fi port, as the phone does (HTTPS, no key). Also returns the served certificate. */
function claimOverWifi(lanPort, code) {
  return new Promise((resolve, reject) => {
    const body = JSON.stringify({ code });
    const req = https.request({
      host: '127.0.0.1', port: lanPort, path: '/api/pair/claim', method: 'POST', agent: false,
      rejectUnauthorized: false, // the agent's self-signed certificate: its fingerprint is checked below instead
      headers: { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(body) },
    }, (res) => {
      const fingerprint = res.socket.getPeerCertificate().fingerprint256.replace(/:/g, '').toLowerCase();
      let text = '';
      res.setEncoding('utf8');
      res.on('data', (d) => { text += d; });
      res.on('end', () => resolve({ status: res.statusCode, text, fingerprint }));
    });
    req.on('error', reject);
    req.end(body);
  });
}

test('the Wi-Fi card says the agent listens on its Wi-Fi port', async ({ page, agent }) => {
  await openSettings(page, agent, '#general');
  await expect(page.locator('#lanStatus')).toHaveText(`Listening on port ${agent.lanPort} (HTTPS)`);
});

test('the pairing dialog shows the agent\'s pairing link and app download as QR codes; closing it cancels the code',
  async ({ page, agent, request }) => {
    const errors = watchErrors(page);
    await openSettings(page, agent, '#general');
    const r = await startPairing(page);
    const lan = await (await request.get('/api/lan')).json();
    expect(r.enabled).toBe(true);
    expect(r.port).toBe(agent.lanPort);

    // the link: this computer's name, the fingerprint of the certificate it serves, its Wi-Fi port and addresses, a code
    const link = new URL(r.link);
    expect(link.protocol).toBe('https:');
    expect(link.pathname).toMatch(/\/pair\/$/);
    const params = new URLSearchParams(link.hash.slice(1));
    expect(params.get('v')).toBe('1');
    expect(params.get('pc')).toBe(r.pc);
    expect(params.get('fp')).toBe(lan.fingerprint);
    expect(params.get('port')).toBe(String(agent.lanPort));
    expect(params.get('h')).toBe(r.hosts.join(','));
    expect(params.get('c')).toMatch(/^\S+$/);
    expect(r.check).toBe(`${lan.fingerprint.slice(0, 4)} ${lan.fingerprint.slice(4, 8)}`.toUpperCase());
    // the app: the release APK of the agent's own version (C# links upstream's release, Rust its updates.repo's)
    const version = reEscape(agent.info.version);
    expect(r.app).toMatch(new RegExp(`^https://github\\.com/[\\w.-]+/[\\w.-]+/releases/download/v${version}/PhoneBox-${version}\\.apk$`));

    const body = page.locator('#modalBody');
    await expect(page.locator('#modal')).toBeVisible();
    await expect(body.locator('h3')).toHaveText(`Pair a phone with ${r.pc}`);
    await expect(body.locator('.pair-qr img')).toHaveCount(2);
    const appQr = body.locator('.pair-qr img[alt="QR code of the app download"]');
    const pairQr = body.locator('.pair-qr img[alt="Pairing QR code"]');
    for (const img of [appQr, pairQr]) {
      await expect(img).toBeVisible();
      await expect(img).toHaveAttribute('src', /^data:image\/svg\+xml;base64,/);
      expect(await img.evaluate((el) => el.naturalWidth > 0 && el.complete)).toBe(true);
    }
    expect(await decodeQr(appQr), 'the app QR code decodes to r.app').toBe(r.app);
    expect(await decodeQr(pairQr), 'the pairing QR code decodes to r.link').toBe(r.link);
    await expect(body.locator('.pair-qr').nth(1)).toContainText(`confirm the code ${r.check}`);
    await expect(body.locator('.pair-qr').nth(1)).toContainText(/Expires in [45]:\d\d\./);
    await expect(body).toContainText(r.hosts.length
      ? `The phone will connect to ${r.hosts.join(', ')} on port ${r.port}.`
      : 'No LAN or Tailscale address was found on this computer');

    const cancel = page.waitForRequest((req) => req.method() === 'POST' && new URL(req.url()).pathname === '/api/pair/cancel');
    await page.locator('#modalClose').click();
    expect(new URL((await cancel).url()).searchParams.get('attempt')).toBe(String(r.attempt));
    await expect(page.locator('#modal')).toBeHidden();
    await (await cancel).response(); // the agent has handled it
    // the cancelled code no longer pairs
    expect((await claimOverWifi(agent.lanPort, codeOf(r.link))).status).toBe(403);
    expect(errors).toEqual([]);
  });

test('a phone that claims the code over Wi-Fi closes the dialog', async ({ page, agent, request }) => {
  await openSettings(page, agent, '#general');
  const r = await startPairing(page);
  await expect(page.locator('#modal')).toBeVisible();
  await clearToast(page);

  const claim = await claimOverWifi(agent.lanPort, codeOf(r.link));
  expect(claim.status).toBe(200);
  const answer = JSON.parse(claim.text); // holds the phone key: compare fields, never print it
  expect(answer.enabled).toBe(true);
  expect(answer.port).toBe(agent.lanPort);
  expect(answer.pc).toBe(r.pc);
  expect(answer.token === agent.config.lan.token, 'the claim answers with the configured phone key').toBe(true);
  // the QR code's fingerprint is the certificate the Wi-Fi port serves
  expect(answer.fingerprint).toBe(claim.fingerprint);
  expect(new URLSearchParams(new URL(r.link).hash.slice(1)).get('fp')).toBe(claim.fingerprint);

  await expect(page.locator('#modal')).toBeHidden();
  await expect(page.locator('#toast')).toHaveText('Phone paired over Wi-Fi');
  expect((await (await request.get('/api/pair/status')).json()).claims).toBe(r.claims + 1);
  // a code works once
  expect((await claimOverWifi(agent.lanPort, codeOf(r.link))).status).toBe(403);
});

// a second agent, started with Wi-Fi off (a worker option cannot change inside a file)
test('with Wi-Fi off, pairing asks to turn it on first', async ({ page }, testInfo) => {
  test.setTimeout(120_000);
  const off = await startAgent({ name: 'pairing-wifi-off', wifi: false });
  try {
    await openSettings(page, off, '#general');
    await expect(page.locator('#lanStatus')).toHaveText('Off');
    await clearToast(page);
    const r = await startPairing(page);
    expect(r).toEqual({ enabled: false, configured: false });
    await expect(page.locator('#toast')).toHaveText(
      'Turn on “Let the phone connect without the USB cable”, click Save, then Restart to apply at the top');
    await expect(page.locator('#toast')).toHaveClass(/\berr\b/);
    await expect(page.locator('#modal')).toBeHidden();
  } catch (e) {
    off.failed = true; // keeps its folder
    await testInfo.attach(`Wi-Fi-off agent output (${off.label})`, { body: off.log(), contentType: 'text/plain' });
    throw e;
  } finally {
    await off.stop();
  }
});
