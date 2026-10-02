#!/usr/bin/env node
// Stand-in for ssh / aws / cloud-sql-proxy / az in the adapter tests. Records its argv to $FAKE_LOG (one JSON line
// per call). Token commands print a fake token; tunnel commands listen on the local port they were asked for, until
// killed. FAKE_FAIL=1 makes a tunnel exit at once (a broken tunnel).
'use strict';
const net = require('node:net');
const fs = require('node:fs');
const argv = process.argv.slice(2);
if (process.env.FAKE_LOG) fs.appendFileSync(process.env.FAKE_LOG, JSON.stringify(argv) + '\n');

const has = (...w) => w.every((x) => argv.includes(x));
if (has('rds', 'generate-db-auth-token') || has('account', 'get-access-token')) {
  process.stdout.write('fake-token/with+chars=\n');
  process.exit(0);
}
if (process.env.FAKE_FAIL === '1') {
  process.stderr.write('fake tunnel: connection refused\n');
  process.exit(255);
}
let port;
const L = argv.indexOf('-L');                                    // ssh -L 127.0.0.1:PORT:host:port
if (L >= 0) port = Number(argv[L + 1].split(':')[1]);
const p = argv.indexOf('--parameters');                          // aws ssm ... localPortNumber=PORT
if (p >= 0) port = Number(/localPortNumber=(\d+)/.exec(argv[p + 1])[1]);
const P = argv.indexOf('--port');                                // cloud-sql-proxy / az bastion tunnel --port PORT
if (port === undefined && P >= 0) port = Number(argv[P + 1]);
if (!port) { process.stderr.write('fake tool: no port in ' + argv.join(' ') + '\n'); process.exit(2); }
const srv = net.createServer((s) => s.end());
srv.listen(port, '127.0.0.1', () => process.stderr.write(`fake tunnel listening on ${port}\n`));
process.on('SIGTERM', () => srv.close(() => process.exit(0)));
