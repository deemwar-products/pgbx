import { useMemo, useState } from 'react';
import { api, type Backup, type DbDetail, type HistoryRow } from '../api';
import { useApp } from '../App';
import { ago, Badge, ErrorCard, Loading, Table, tone, ts, useApi } from '../ui';

const SETTINGS: [string, string][] = [
  ['schedule', 'schedule'],
  ['next_backup_at', 'next backup'],
  ['retention', 'retention'],
  ['backups_kept', 'backups kept'],
  ['verify_schedule', 'restore tests'],
  ['last_verified_at', 'last restore test'],
  ['last_verify_result', 'restore test result'],
  ['data_scope', 'data scope'],
  ['location', 'location'],
  ['load_gate', 'load gate'],
  ['suggested_schedule', 'suggested schedule'],
  ['queued_jobs', 'queued jobs'],
  ['job_progress', 'running job'],
  ['waiting_reason', 'waiting because'],
  ['paused_reason', 'paused because'],
];

export function DbScreen({ name }: { name: string }) {
  const { gen, session, confirm, profileFlag } = useApp();
  const d = useApi<DbDetail>(`/api/db/${encodeURIComponent(name)}`, [gen], 15000);
  if (d.error)
    return (
      <>
        <Back />
        <ErrorCard error={d.error} title={`Cannot read ${name}`} />
      </>
    );
  if (!d.data) return <Loading what={name} />;
  const s = d.data.status || {};
  const act = (kind: 'backup' | 'verify') =>
    confirm({
      title: kind === 'backup' ? `Back up ${name} now?` : `Run a restore test of ${name} now?`,
      body:
        kind === 'backup' ? (
          <p>Queues a backup of <b>{name}</b> to S3. It runs when a job slot is free; the live database is only read.</p>
        ) : (
          <p>
            Restores the newest backup of <b>{name}</b> into a scratch database, checks it and drops it. The live database is not touched.
          </p>
        ),
      cli: `pgbx ${kind === 'backup' ? 'now' : 'verify'} --db ${name}${profileFlag}`,
      confirm: kind === 'backup' ? 'Back up now' : 'Run restore test',
      run: () => api(`/api/action/${kind}`, { db: name }),
    });
  return (
    <>
      <Back />
      <div className="card">
        <div className="row spread wrap-row">
          <h1 className="h1">{name}</h1>
          <Badge>{s.state || 'unknown'}</Badge>
        </div>
        {s.last_error && (
          <p className={tone(s.state) === 'bad' ? 'bad' : 'mute'}>
            last error {ago(s.last_error_at)}: {s.last_error}
          </p>
        )}
        <p>
          Last backup{' '}
          {s.last_backup_at ? (
            <>
              <b>{ago(s.last_backup_at)}</b> ({s.last_backup_size}) <span className="mute small">{ts(s.last_backup_at)}</span>
            </>
          ) : (
            <b className="warn">none yet</b>
          )}
        </p>
        <div className="row gap">
          <a className="btn" href={`#/restore/${encodeURIComponent(name)}`}>
            Restore helper
          </a>
          <a className="btn" href={`#/query/${encodeURIComponent(name)}`}>
            Query
          </a>
          {session.actions.includes('backup') && (
            <button type="button" className="btn primary" onClick={() => act('backup')}>
              Back up now
            </button>
          )}
          {session.actions.includes('verify') && (
            <button type="button" className="btn" onClick={() => act('verify')}>
              Verify now
            </button>
          )}
        </div>
      </div>

      <div className="card">
        <h2>Schedule and retention</h2>
        <p className="mute small">Read-only here. Change them with the CLI (pgbx schedule, pgbx retention, pgbx scope).</p>
        <dl className="kv">
          {SETTINGS.filter(([k]) => s[k] != null && s[k] !== '').map(([k, label]) => (
            <div key={k}>
              <dt>{label}</dt>
              <dd>{/_at$/.test(k) ? `${ts(s[k])} (${ago(s[k])})` : String(s[k])}</dd>
            </div>
          ))}
        </dl>
      </div>

      <div className="card">
        <h2>Backups kept ({d.data.backups.length})</h2>
        <Table<Backup>
          rows={d.data.backups}
          empty="no backups yet"
          cols={[
            { h: 'id', c: (b) => b.id },
            { h: 'taken', c: (b) => <span title={ts(b.taken_at)}>{ago(b.taken_at)}</span> },
            { h: 'trigger', c: (b) => b.trigger },
            { h: 'size', c: (b) => b.size },
            { h: 's3 key', wide: true, c: (b) => <code>{b.s3_key}</code> },
            {
              h: '',
              c: (b) => (
                <a className="btn small" href={`#/restore/${encodeURIComponent(name)}/${b.id}`}>
                  Restore…
                </a>
              ),
            },
          ]}
        />
      </div>

      <Timeline rows={d.data.history} />
    </>
  );
}

function Back() {
  return (
    <p>
      <a href="#/overview" className="small">
        ← all databases
      </a>
    </p>
  );
}

function Timeline({ rows }: { rows: HistoryRow[] }) {
  const [kind, setKind] = useState('');
  const [state, setState] = useState('');
  const kinds = useMemo(() => [...new Set(rows.map((r) => r.kind))].sort(), [rows]);
  const states = useMemo(() => [...new Set(rows.map((r) => r.state))].sort(), [rows]);
  const shown = rows.filter((r) => (!kind || r.kind === kind) && (!state || r.state === state));
  return (
    <div className="card">
      <div className="row spread wrap-row">
        <h2>History ({shown.length})</h2>
        <div className="row gap">
          <select value={kind} onChange={(e) => setKind(e.target.value)} aria-label="kind">
            <option value="">every kind</option>
            {kinds.map((k) => (
              <option key={k}>{k}</option>
            ))}
          </select>
          <select value={state} onChange={(e) => setState(e.target.value)} aria-label="state">
            <option value="">every state</option>
            {states.map((k) => (
              <option key={k}>{k}</option>
            ))}
          </select>
        </div>
      </div>
      <ol className="timeline">
        {shown.map((r) => (
          <li key={r.id} className={`t-${tone(r.state)}`}>
            <div className="row spread wrap-row">
              <span>
                <b>{r.kind}</b> <Badge>{r.state}</Badge> <span className="mute small">#{r.id} · {r.trigger}{r.who && r.who !== r.trigger ? ` · by ${r.who}` : ''}</span>
              </span>
              <span className="mute small" title={ts(r.at)}>
                {ago(r.at)}
              </span>
            </div>
            {r.error && <p className={r.state === 'cancelled' ? 'mute small wrap' : 'bad small wrap'}>{r.error}</p>}
            {r.kind === 'restore' && r.params?.into_db && <p className="small">into {String(r.params.into_db)}{r.params.at ? ` at ${ts(r.params.at)}` : ''}</p>}
            {r.kind === 'config' && r.params && <p className="mute small wrap">{summarize(r.params)}</p>}
            {r.s3_key && <p className="mute small wrap"><code>{r.s3_key}</code></p>}
          </li>
        ))}
        {!shown.length && <p className="mute">nothing here</p>}
      </ol>
    </div>
  );
}

function summarize(p: Record<string, unknown>): string {
  return Object.entries(p)
    .filter(([k]) => !/url|key|secret|token/i.test(k))
    .map(([k, v]) => `${k}: ${typeof v === 'object' ? JSON.stringify(v) : String(v)}`)
    .join(' · ');
}
