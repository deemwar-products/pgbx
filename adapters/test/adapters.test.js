// Protocol tests for the example adapters, with fake vendor tools (no cloud accounts needed).
// Run: node --test adapters/test/
'use strict';
const test = require('node:test');
const assert = require('node:assert');
const { spawn } = require('node:child_process');
const net = require('node:net');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const root = path.join(__dirname, '..');
const fake = path.join(__dirname, 'fake-tool.js');
fs.chmodSync(fake, 0o755);
const fakeEnv = { PGBX_SSH_BIN: fake, PGBX_AWS_BIN: fake, PGBX_CLOUD_SQL_PROXY_BIN: fake, PGBX_AZ_BIN: fake };

/** Start an adapter, send start, return {child, result, stderr(), log()} once its single stdout line arrived. */
function startAdapter(script, name, config, extraEnv = {}) {
  const logFile = path.join(os.tmpdir(), `pgbx-adapter-${process.pid}-${Math.random().toString(36).slice(2)}.log`);
  const child = spawn(process.execPath, [path.join(root, script)], {
    env: { ...process.env, ...fakeEnv, FAKE_LOG: logFile, ...extraEnv },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  let out = '', err = '';
  child.stderr.on('data', (d) => (err += d));
  child.stdin.write(JSON.stringify({ action: 'start', name, config }) + '\n');
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('no result line within 15 s; stderr: ' + err)), 15000);
    child.stdout.on('data', (d) => {
      out += d;
      const nl = out.indexOf('\n');
      if (nl >= 0) {
        clearTimeout(t);
        resolve({
          child, result: JSON.parse(out.slice(0, nl)), rest: () => out.slice(nl + 1), stderr: () => err,
          log: () => (fs.existsSync(logFile) ? fs.readFileSync(logFile, 'utf8').trim().split('\n').map((l) => JSON.parse(l)) : []),
        });
      }
    });
  });
}

const exited = (child) => new Promise((r) => (child.exitCode !== null ? r(child.exitCode) : child.once('exit', (c) => r(c))));
const portOf = (url) => Number(new URL(url).port);
const accepts = (port) => new Promise((r) => { const c = net.connect(port, '127.0.0.1'); c.once('connect', () => { c.destroy(); r(true); }); c.once('error', () => r(false)); });

test('ssh: ready line, tunnel up, stop tears it down', async () => {
  const a = await startAdapter('ssh/ssh-adapter.js', 'prod', { target: 'ops@db1', jump: 'bastion', ssh_port: 2222, user: 'app', password: 'p@ss:w/rd', dbname: 'shop' });
  assert.equal(a.result.state, 'ready');
  assert.equal(a.result.name, 'prod');
  const u = new URL(a.result.url);
  assert.equal(u.hostname, '127.0.0.1');
  assert.equal(decodeURIComponent(u.password), 'p@ss:w/rd');
  assert.equal(u.pathname, '/shop');
  assert.ok(await accepts(portOf(a.result.url)), 'tunnel port accepts');
  const argv = a.log()[0];
  assert.ok(argv.includes('-N') && argv.includes('ops@db1') && argv.includes('-J') && argv.includes('bastion') && argv.includes('2222'));
  assert.ok(!a.stderr().includes('p@ss'), 'password never in stderr');
  a.child.stdin.write('{"action":"stop"}\n');
  assert.equal(await exited(a.child), 0);
  assert.equal(a.rest(), '', 'stdout carries exactly one line');
  assert.equal(await accepts(portOf(a.result.url)), false, 'tunnel gone after stop');
});

test('ssh: stdin EOF also means stop', async () => {
  const a = await startAdapter('ssh/ssh-adapter.js', 'prod', { target: 'ops@db1' });
  assert.equal(a.result.state, 'ready');
  a.child.stdin.end();
  assert.equal(await exited(a.child), 0);
  assert.equal(await accepts(portOf(a.result.url)), false);
});

test('ssh: missing target -> error state naming the key, exit 1', async () => {
  const a = await startAdapter('ssh/ssh-adapter.js', 'prod', {});
  assert.match(a.result.state, /^error: ssh: profile config needs "target"/);
  assert.equal(a.result.url, '');
  assert.equal(await exited(a.child), 1);
});

test('ssh: broken tunnel -> error state, exit 1', async () => {
  const a = await startAdapter('ssh/ssh-adapter.js', 'prod', { target: 'ops@db1' }, { FAKE_FAIL: '1' });
  assert.match(a.result.state, /^error: tunnel process exited/);
  assert.equal(await exited(a.child), 1);
});

test('aws: SSM port forwarding to an RDS host, IAM token as password', async () => {
  const a = await startAdapter('aws/aws-adapter.js', 'prod-eu', { target: 'i-0abc', db_host: 'shop.xyz.eu-west-1.rds.amazonaws.com', region: 'eu-west-1', aws_profile: 'acme', user: 'app', iam_auth: 'true', dbname: 'shop' });
  assert.equal(a.result.state, 'ready');
  const u = new URL(a.result.url);
  assert.equal(decodeURIComponent(u.password), 'fake-token/with+chars=');
  assert.equal(u.searchParams.get('sslmode'), 'require');
  const [tok, ssm] = a.log();
  assert.ok(tok.includes('generate-db-auth-token') && tok.includes('--username') && tok.includes('app'));
  assert.ok(ssm.includes('AWS-StartPortForwardingSessionToRemoteHost') && ssm.includes('i-0abc') && ssm.includes('--profile'));
  assert.ok(!a.stderr().includes('fake-token'), 'token never in stderr');
  a.child.stdin.write('{"action":"stop"}\n');
  assert.equal(await exited(a.child), 0);
});

test('aws: Postgres on the instance itself uses the local document', async () => {
  const a = await startAdapter('aws/aws-adapter.js', 'ec2', { target: 'i-0abc', user: 'app', password: 'x' });
  assert.equal(a.result.state, 'ready');
  assert.ok(a.log()[0].includes('AWS-StartPortForwardingSession'));
  a.child.stdin.end();
  await exited(a.child);
});

test('gcp: cloud-sql-proxy on a free local port', async () => {
  const a = await startAdapter('gcp/gcp-adapter.js', 'gcp-prod', { instance: 'acme:europe-west1:shop', iam_auth: 'true', user: 'me@acme.com', dbname: 'shop' });
  assert.equal(a.result.state, 'ready');
  const argv = a.log()[0];
  assert.equal(argv[0], 'acme:europe-west1:shop');
  assert.ok(argv.includes('--auto-iam-authn') && argv.includes('127.0.0.1'));
  assert.equal(new URL(a.result.url).searchParams.get('sslmode'), 'disable');
  a.child.stdin.write('{"action":"stop"}\n');
  assert.equal(await exited(a.child), 0);
});

test('azure: direct mode needs no tunnel; Entra token as password', async () => {
  const a = await startAdapter('azure/azure-adapter.js', 'az-prod', { host: 'shop.postgres.database.azure.com', user: 'me@acme.com', entra_auth: 'true', dbname: 'shop' });
  assert.equal(a.result.state, 'ready');
  const u = new URL(a.result.url);
  assert.equal(u.hostname, 'shop.postgres.database.azure.com');
  assert.equal(u.searchParams.get('sslmode'), 'require');
  assert.equal(decodeURIComponent(u.password), 'fake-token/with+chars=');
  a.child.stdin.end();
  assert.equal(await exited(a.child), 0);
});

test('azure: bastion tunnel', async () => {
  const a = await startAdapter('azure/azure-adapter.js', 'az-vm', { bastion: 'b1', resource_group: 'rg', target_resource_id: '/subscriptions/x/vm1', user: 'app', password: 'x' });
  assert.equal(a.result.state, 'ready');
  assert.ok(a.log()[0].includes('bastion') && a.log()[0].includes('tunnel'));
  assert.ok(await accepts(portOf(a.result.url)));
  a.child.stdin.write('{"action":"stop"}\n');
  assert.equal(await exited(a.child), 0);
});
