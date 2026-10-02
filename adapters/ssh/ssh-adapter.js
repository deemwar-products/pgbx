#!/usr/bin/env node
// pgbx example adapter: reach Postgres through the system `ssh` (keys, agent, ~/.ssh/config, ProxyJump are ssh's).
// Profile config keys: target (user@host, required), ssh_port, jump, pg_host (default localhost on the server),
// pg_port (5432), user, password, dbname, sslmode, ready_timeout_ms (20000).
'use strict';
const { run, freePort, waitForPort, startTunnel, pgUrl, killChild, need } = require('../lib/adapter');

run(async (name, c) => {
  const target = need(c, 'target', 'ssh');
  const port = await freePort();
  const args = ['-N', '-o', 'ExitOnForwardFailure=yes', '-o', 'BatchMode=yes', '-o', 'ServerAliveInterval=30',
    '-L', `127.0.0.1:${port}:${c.pg_host || 'localhost'}:${c.pg_port || 5432}`];
  if (c.ssh_port) args.push('-p', String(c.ssh_port));
  if (c.jump) args.push('-J', String(c.jump));
  args.push(target);
  const child = startTunnel(process.env.PGBX_SSH_BIN || 'ssh', args);
  try {
    await waitForPort(port, Number(c.ready_timeout_ms) || 20000, child);
  } catch (e) {
    await killChild(child);
    throw e;
  }
  return {
    url: pgUrl({ user: c.user, password: c.password, port, dbname: c.dbname, sslmode: c.sslmode }),
    cleanup: () => killChild(child),
  };
});
