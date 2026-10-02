#!/usr/bin/env node
// pgbx example adapter: RDS / EC2 Postgres through AWS Systems Manager port forwarding (no open ports, no bastion),
// using the user's own `aws` CLI and credentials (plus the session-manager-plugin).
// Profile config keys: target (an SSM-managed instance id, required), db_host (the RDS endpoint as seen from that
// instance; default localhost = Postgres on the instance itself), db_port (5432), region, aws_profile,
// user, password, dbname, sslmode, iam_auth ("true" = mint an RDS IAM auth token instead of a password),
// ready_timeout_ms (30000).
'use strict';
const { run, freePort, waitForPort, startTunnel, capture, pgUrl, killChild, need } = require('../lib/adapter');

const aws = () => process.env.PGBX_AWS_BIN || 'aws';

run(async (name, c) => {
  const target = need(c, 'target', 'aws');
  const dbHost = c.db_host || 'localhost';
  const dbPort = String(c.db_port || 5432);
  const common = [];
  if (c.region) common.push('--region', String(c.region));
  if (c.aws_profile) common.push('--profile', String(c.aws_profile));

  let password = c.password;
  if (String(c.iam_auth) === 'true') {
    need(c, 'user', 'aws');
    // a 15-minute token; used once to connect, never printed by this adapter
    password = await capture(aws(), ['rds', 'generate-db-auth-token', '--hostname', dbHost, '--port', dbPort,
      '--username', String(c.user), ...common]);
  }

  const port = await freePort();
  const doc = dbHost === 'localhost' ? 'AWS-StartPortForwardingSession' : 'AWS-StartPortForwardingSessionToRemoteHost';
  const params = dbHost === 'localhost'
    ? `portNumber=${dbPort},localPortNumber=${port}`
    : `host=${dbHost},portNumber=${dbPort},localPortNumber=${port}`;
  const child = startTunnel(aws(), ['ssm', 'start-session', '--target', target, '--document-name', doc,
    '--parameters', params, ...common]);
  try {
    await waitForPort(port, Number(c.ready_timeout_ms) || 30000, child);
  } catch (e) {
    await killChild(child);
    throw e;
  }
  return {
    url: pgUrl({ user: c.user, password, port, dbname: c.dbname, sslmode: c.sslmode || (String(c.iam_auth) === 'true' ? 'require' : undefined) }),
    cleanup: () => killChild(child),
  };
});
