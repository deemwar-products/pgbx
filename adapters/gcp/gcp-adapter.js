#!/usr/bin/env node
// pgbx example adapter: Cloud SQL for PostgreSQL through Google's `cloud-sql-proxy` (v2), with the user's own
// gcloud / Application Default Credentials.
// Profile config keys: instance ("project:region:instance", required), iam_auth ("true" = --auto-iam-authn, the
// user is then the IAM principal), private_ip ("true"), user, password, dbname, ready_timeout_ms (30000).
'use strict';
const { run, freePort, waitForPort, startTunnel, pgUrl, killChild, need } = require('../lib/adapter');

run(async (name, c) => {
  const instance = need(c, 'instance', 'gcp');
  const port = await freePort();
  const args = [instance, '--port', String(port), '--address', '127.0.0.1'];
  if (String(c.iam_auth) === 'true') args.push('--auto-iam-authn');
  if (String(c.private_ip) === 'true') args.push('--private-ip');
  const child = startTunnel(process.env.PGBX_CLOUD_SQL_PROXY_BIN || 'cloud-sql-proxy', args);
  try {
    await waitForPort(port, Number(c.ready_timeout_ms) || 30000, child);
  } catch (e) {
    await killChild(child);
    throw e;
  }
  // the proxy encrypts to the instance itself, so the local hop is plain TCP
  return {
    url: pgUrl({ user: c.user, password: c.password, port, dbname: c.dbname, sslmode: 'disable' }),
    cleanup: () => killChild(child),
  };
});
