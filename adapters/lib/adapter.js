// Shared helper for pgbx's example adapters (ADR 0003). No dependencies beyond Node's standard library.
//
// Protocol v1 (line-delimited JSON):
//   stdin  <- {"action":"start","name":"prod","config":{...}}   (config has $VARs already expanded by pgbx)
//   stdout -> exactly ONE line: {"url":"postgres://...","state":"ready","name":"prod"}  (or state "error: ...")
//   stdin  <- {"action":"stop"}   or stdin closes  -> clean up and exit
// stdout carries only that result line; everything else goes to stderr.

'use strict';
const net = require('node:net');
const { spawn } = require('node:child_process');
const readline = require('node:readline');

const log = (...a) => process.stderr.write(a.join(' ') + '\n');

/** A free TCP port on 127.0.0.1 (the OS picks it). */
function freePort() {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.unref();
    s.on('error', reject);
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
}

/** Resolve once 127.0.0.1:port accepts a connection, or reject after timeoutMs / when `child` exits. */
function waitForPort(port, timeoutMs, child) {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + timeoutMs;
    let done = false;
    const finish = (err) => { if (!done) { done = true; err ? reject(err) : resolve(); } };
    if (child) child.once('exit', (code) => finish(new Error(`tunnel process exited (${code}) before 127.0.0.1:${port} was ready`)));
    const attempt = () => {
      if (done) return;
      const c = net.connect(port, '127.0.0.1');
      c.once('connect', () => { c.destroy(); finish(); });
      c.once('error', () => {
        c.destroy();
        if (Date.now() > deadline) finish(new Error(`127.0.0.1:${port} not ready after ${timeoutMs} ms`));
        else setTimeout(attempt, 200);
      });
    };
    attempt();
  });
}

/** Start a long-running helper process (ssh -N -L ..., cloud-sql-proxy, ...) whose output goes to our stderr. */
function startTunnel(cmd, args, env) {
  log(`starting: ${cmd} ${args.join(' ')}`);
  const child = spawn(cmd, args, { stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, ...(env || {}) } });
  child.stdout.on('data', (d) => process.stderr.write(d));
  child.stderr.on('data', (d) => process.stderr.write(d));
  child.on('error', (e) => log(`cannot start ${cmd}: ${e.message}`));
  return child;
}

/** Run a command to completion and return its trimmed stdout (for tokens: never logged). */
function capture(cmd, args) {
  return new Promise((resolve, reject) => {
    const child = spawn(cmd, args, { stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '', err = '';
    child.stdout.on('data', (d) => (out += d));
    child.stderr.on('data', (d) => (err += d));
    child.on('error', (e) => reject(new Error(`cannot run ${cmd}: ${e.message}`)));
    child.on('close', (code) => (code === 0 ? resolve(out.trim()) : reject(new Error(`${cmd} exited ${code}: ${err.trim().slice(0, 300)}`))));
  });
}

/** postgres://user:pass@host:port/db?sslmode=... with every part URL-encoded. */
function pgUrl({ user, password, host = '127.0.0.1', port, dbname = 'postgres', sslmode }) {
  const auth = user ? encodeURIComponent(user) + (password ? ':' + encodeURIComponent(password) : '') + '@' : '';
  const q = sslmode ? `?sslmode=${encodeURIComponent(sslmode)}` : '';
  return `postgres://${auth}${host}:${port}/${encodeURIComponent(dbname)}${q}`;
}

/**
 * Run an adapter. `connect(name, config)` returns {url, cleanup?}. Prints the single result line, then waits for
 * {"action":"stop"} or stdin EOF, runs cleanup and exits.
 */
function run(connect) {
  let started = false, cleanup = null, stopping = false;
  const out = (o) => process.stdout.write(JSON.stringify(o) + '\n');
  const stop = async (code = 0) => {
    if (stopping) return;
    stopping = true;
    try { if (cleanup) await cleanup(); } catch (e) { log(`cleanup: ${e.message}`); }
    process.exit(code);
  };
  const rl = readline.createInterface({ input: process.stdin });
  rl.on('line', async (line) => {
    let msg;
    try { msg = JSON.parse(line); } catch { log(`ignoring a non-JSON line on stdin`); return; }
    if (msg.action === 'stop') return stop(0);
    if (msg.action !== 'start' || started) return;
    started = true;
    const name = msg.name || '';
    try {
      const r = await connect(name, msg.config || {});
      cleanup = r.cleanup || null;
      out({ url: r.url, state: 'ready', name });
    } catch (e) {
      out({ url: '', state: `error: ${e.message}`, name });
      await stop(1);
    }
  });
  rl.on('close', () => stop(0)); // stdin EOF = stop (pgbx went away)
  for (const sig of ['SIGTERM', 'SIGINT']) process.on(sig, () => stop(0));
}

/** Kill a child and wait (bounded) for it to exit. */
function killChild(child, ms = 3000) {
  return new Promise((resolve) => {
    if (!child || child.exitCode !== null || child.signalCode) return resolve();
    const t = setTimeout(() => { try { child.kill('SIGKILL'); } catch {} resolve(); }, ms);
    child.once('exit', () => { clearTimeout(t); resolve(); });
    try { child.kill('SIGTERM'); } catch { clearTimeout(t); resolve(); }
  });
}

/** `config[key]` or throw a clear error naming the key (never a value). */
function need(config, key, adapter) {
  const v = config[key];
  if (v === undefined || v === null || v === '') throw new Error(`${adapter}: profile config needs "${key}"`);
  return String(v);
}

module.exports = { run, freePort, waitForPort, startTunnel, capture, pgUrl, killChild, need, log };
