// Starts and stops one live agent for the browser suite, as tools/parity/parity.py does: a temporary folder holding
// config.json (fixture.js) and, with Wi-Fi on, the fixture certificate; two free ports; PHONEBOX_AGENT_CMD with
// --config and --port, and HOME / XDG_CONFIG_HOME / XDG_DATA_HOME in that folder, so it never touches an installed
// agent or a phone. Ready once GET /api/info answers 200 (up to 60 s). Stopped by its own PID only: SIGTERM, then
// SIGKILL after 10 s. The folder (with the agent's output in stdout.log) is kept when a test failed or PHONEBOX_KEEP=1.
const { spawn } = require('child_process');
const fs = require('fs');
const http = require('http');
const net = require('net');
const os = require('os');
const path = require('path');
const { liveConfig } = require('./fixture');

const ROOT = path.resolve(__dirname, '..', '..', '..');
const CERT = path.join(ROOT, 'tools', 'parity', 'fixtures', 'lan-cert.pfx');
const READY_MS = 60_000;
const STOP_MS = 10_000;

/** Splits a command line the way Python's shlex.split does (POSIX rules), as parity.py splits its agent commands. */
function splitCommand(text) {
  const out = [];
  let cur = '';
  let word = false;
  let quote = null;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quote === "'") {
      if (c === "'") quote = null; else cur += c;
    } else if (quote === '"') {
      if (c === '"') quote = null;
      else if (c === '\\' && i + 1 < text.length && '"\\$`\n'.includes(text[i + 1])) cur += text[++i];
      else cur += c;
    } else if (/\s/.test(c)) {
      if (word) { out.push(cur); cur = ''; word = false; }
    } else {
      word = true;
      if (c === "'" || c === '"') quote = c;
      else if (c === '\\' && i + 1 < text.length) cur += text[++i];
      else cur += c;
    }
  }
  if (quote) throw new Error(`PHONEBOX_AGENT_CMD has an unclosed ${quote} quote`);
  if (word) out.push(cur);
  return out;
}

/** The agent's command line from PHONEBOX_AGENT_CMD; a relative program path is taken from the repository root. */
function agentCommand() {
  const cmd = process.env.PHONEBOX_AGENT_CMD;
  if (!cmd || !cmd.trim()) {
    throw new Error('PHONEBOX_AGENT_CMD is not set. Give the command line of the agent to test, for example\n'
      + '  PHONEBOX_AGENT_CMD=agent-rs/target/release/PhoneBoxAgent npx playwright test -c live\n'
      + '(run in tools/ui-tests; a relative program path is taken from the repository root)');
  }
  const argv = splitCommand(cmd);
  if (!path.isAbsolute(argv[0]) && /[\\/]/.test(argv[0])) argv[0] = path.resolve(ROOT, argv[0]);
  return argv;
}

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

function get(port, urlPath) {
  return new Promise((resolve) => {
    const req = http.get({ host: '127.0.0.1', port, path: urlPath, timeout: 2000 }, (res) => {
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (d) => { body += d; });
      res.on('end', () => resolve({ status: res.statusCode, body }));
      res.on('error', () => resolve(null));
    });
    req.on('timeout', () => req.destroy(new Error('timeout')));
    req.on('error', () => resolve(null));
  });
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const tail = (text, lines = 40) => text.split('\n').slice(-lines).join('\n');

/**
 * Starts an agent. `name` labels its folder; `wifi: false` starts it with Wi-Fi off. The result has `url` (the local
 * port, for baseURL), `port`, `lanPort`, `dir`, `configPath`, `config` (the fixture it started with), `info`
 * (its first /api/info answer), `log()`, `failed` (set it to keep the folder) and `stop()`.
 */
async function startAgent({ name = 'live', wifi = true } = {}) {
  const argv = agentCommand();
  const label = process.env.PHONEBOX_AGENT_LABEL || path.basename(argv[0]);
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `phonebox-ui-live-${name}-`));
  const port = await freePort();
  let lanPort = await freePort();
  while (lanPort === port) lanPort = await freePort();
  const config = liveConfig({ lanPort, wifi });
  const configPath = path.join(dir, 'config.json');
  fs.writeFileSync(configPath, JSON.stringify(config, null, 2));
  if (wifi) fs.copyFileSync(CERT, path.join(dir, 'lan-cert.pfx'));
  const logPath = path.join(dir, 'stdout.log');
  const logFd = fs.openSync(logPath, 'w');
  const env = { ...process.env, HOME: dir, XDG_CONFIG_HOME: dir, XDG_DATA_HOME: dir };
  const proc = spawn(argv[0], [...argv.slice(1), '--config', configPath, '--port', String(port)],
    { env, stdio: ['ignore', logFd, logFd] });
  fs.closeSync(logFd); // the agent has its own copy of the handle
  let exited = null;
  const exit = new Promise((resolve) => {
    proc.once('exit', (code, signal) => { exited = exited || `exit code ${code}, signal ${signal}`; resolve(); });
    proc.once('error', (e) => { exited = exited || `could not start: ${e.message}`; resolve(); });
  });
  // a worker that dies (crash, timeout) must not leave its agent running: kill this one PID, nothing else
  const killOnExit = () => { if (!exited) { try { proc.kill('SIGKILL'); } catch (_) { /* gone already */ } } };
  process.once('exit', killOnExit);
  const log = () => { try { return fs.readFileSync(logPath, 'utf8'); } catch (_) { return ''; } };

  const agent = {
    name, label, dir, configPath, port, lanPort, wifi, config, pid: proc.pid,
    url: `http://127.0.0.1:${port}/`,
    info: null,
    failed: false,
    log,
    async stop() {
      if (!exited) {
        proc.kill('SIGTERM'); // by PID: only the process this suite started
        let timer;
        const late = await Promise.race([exit.then(() => false), new Promise((r) => { timer = setTimeout(r, STOP_MS, true); })]);
        clearTimeout(timer);
        if (late) { proc.kill('SIGKILL'); await exit; }
      }
      process.removeListener('exit', killOnExit);
      if (agent.failed || process.env.PHONEBOX_KEEP === '1') {
        process.stderr.write(`[${label}] agent folder kept: ${dir}\n`);
      } else {
        fs.rmSync(dir, { recursive: true, force: true });
      }
    },
  };

  const deadline = Date.now() + READY_MS;
  for (;;) {
    if (exited) {
      agent.failed = true;
      await agent.stop();
      throw new Error(`[${label}] the agent (${argv.join(' ')}) ended before it answered (${exited}). Its output:\n`
        + `${tail(log())}\nFolder kept: ${dir}`);
    }
    const r = await get(port, '/api/info');
    if (r && r.status === 200) {
      agent.info = JSON.parse(r.body);
      return agent;
    }
    if (Date.now() > deadline) {
      agent.failed = true;
      await agent.stop();
      throw new Error(`[${label}] the agent did not answer GET /api/info on port ${port} within ${READY_MS / 1000} s. `
        + `Its output:\n${tail(log())}\nFolder kept: ${dir}`);
    }
    await sleep(300);
  }
}

module.exports = { startAgent, splitCommand, agentCommand };
