#!/usr/bin/env node
// pgbx example adapter: Azure, with the user's own `az` CLI login.
// Two ways in:
//   - direct (Azure Database for PostgreSQL flexible server reachable from here): config host, no tunnel;
//   - bastion (Postgres on a VM, or a private endpoint reachable from one): `az network bastion tunnel`.
// Profile config keys: host (direct mode), bastion + resource_group + target_resource_id (bastion mode),
// db_port (5432), user, password, dbname, sslmode (default require), entra_auth ("true" = use an Entra ID access
// token from `az account get-access-token --resource-type oss-rdbms` as the password), ready_timeout_ms (30000).
'use strict';
const { run, freePort, waitForPort, startTunnel, capture, pgUrl, killChild, need } = require('../lib/adapter');

const az = () => process.env.PGBX_AZ_BIN || 'az';

run(async (name, c) => {
  let password = c.password;
  if (String(c.entra_auth) === 'true') {
    need(c, 'user', 'azure');
    password = await capture(az(), ['account', 'get-access-token', '--resource-type', 'oss-rdbms', '--query', 'accessToken', '-o', 'tsv']);
  }
  const sslmode = c.sslmode || 'require';

  if (c.host && !c.bastion) {
    return { url: pgUrl({ user: c.user, password, host: String(c.host), port: c.db_port || 5432, dbname: c.dbname, sslmode }) };
  }
  const bastion = need(c, 'bastion', 'azure');
  const rg = need(c, 'resource_group', 'azure');
  const targetId = need(c, 'target_resource_id', 'azure');
  const port = await freePort();
  const child = startTunnel(az(), ['network', 'bastion', 'tunnel', '--name', bastion, '--resource-group', rg,
    '--target-resource-id', targetId, '--resource-port', String(c.db_port || 5432), '--port', String(port)]);
  try {
    await waitForPort(port, Number(c.ready_timeout_ms) || 30000, child);
  } catch (e) {
    await killChild(child);
    throw e;
  }
  return {
    url: pgUrl({ user: c.user, password, port, dbname: c.dbname, sslmode }),
    cleanup: () => killChild(child),
  };
});
