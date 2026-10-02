import { useEffect, useMemo, useState } from 'react';
import { api, type Json, type Overview, type OverviewDb, type QueueJob, type Suggestion } from '../api';
import { useApp } from '../App';
import { ago, Badge, CommandLine, ErrorCard, Loading, Table, tone, ts, useApi, type Col } from '../ui';

export function OverviewScreen() {
  const { gen } = useApp();
  const ov = useApi<Overview>('/api/overview', [gen], 30000);
  if (ov.error) return <ErrorCard error={ov.error} title="Cannot read this server" />;
  if (!ov.data) return <Loading what="overview" />;
  const o = ov.data;
  return (
    <>
      <ConnectionLine o={o} />
      {o.backups === 'off' ? <BackupsOff o={o} /> : <Databases dbs={o.databases} />}
      {o.backups === 'on' && (
        <div className="cols">
          <Queue />
          <LoadGate />
        </div>
      )}
      {o.backups === 'on' && o.databases.length > 0 && <QuietWindow o={o} />}
    </>
  );
}

function ConnectionLine({ o }: { o: Overview }) {
  const c = o.connection;
  return (
    <p className="mute small conn-line">
      {c.server_name ? <>server <b>{c.server_name}</b> · </> : null}
      role <b>{c.role}</b> · read-only session {c.read_only} · server time {ts(c.server_time)}
      {c.server_version ? <> · Postgres {c.server_version}</> : null}
    </p>
  );
}

function BackupsOff({ o }: { o: Overview }) {
  return (
    <>
      <div className="card notice">
        <h2>Backups are off on this server</h2>
        <p>{o.info}</p>
        <p className="mute">
          The pgbx extension is not installed here, so nothing is being backed up. Everything else works: browse the databases, run read-only
          queries, check health.
        </p>
        {o.next_steps?.map((s) => (
          <CommandLine key={s} cmd={s.replace(/^optional, to turn on backups: on the database server run `?/, '').replace(/`/g, '')} label="Copy" />
        ))}
      </div>
      <div className="card">
        <h2>Databases ({o.databases.length})</h2>
        <Table<OverviewDb>
          rows={o.databases}
          cols={[
            { h: 'database', c: (r) => <b>{r.database}</b> },
            { h: 'size', c: (r) => r.size },
            { h: 'backed up', c: () => <Badge t="mute">off</Badge> },
            {
              h: '',
              c: (r) => (
                <a className="btn small" href={`#/query/${encodeURIComponent(r.database)}`}>
                  Query
                </a>
              ),
            },
          ]}
        />
      </div>
    </>
  );
}

/** 'ok: restored 4 tables from X.dump' -> a short badge plus the detail */
function Verify({ text }: { text: string }) {
  const [head, ...rest] = text.split(':');
  return (
    <span>
      <Badge t={tone(head)}>{head}</Badge> {rest.length > 0 && <span className="mute small">{rest.join(':').trim()}</span>}
    </span>
  );
}

function stateOf(r: OverviewDb): string {
  return r.state || 'unknown';
}

function Databases({ dbs }: { dbs: OverviewDb[] }) {
  const failing = dbs.filter((d) => tone(d.state) === 'bad').length;
  const backed = dbs.filter((d) => d.last_backup_at).length;
  const cols: Col<OverviewDb>[] = [
    {
      h: 'database',
      c: (r) => (
        <a href={`#/db/${encodeURIComponent(r.database)}`} className="dblink">
          {r.database}
        </a>
      ),
    },
    { h: 'state', c: (r) => <Badge>{stateOf(r)}</Badge> },
    {
      h: 'last backup',
      c: (r) =>
        r.last_backup_at ? (
          <span title={ts(r.last_backup_at)}>
            {ago(r.last_backup_at)} <span className="mute">· {r.last_backup_size}</span>
          </span>
        ) : (
          <span className="warn">none yet</span>
        ),
    },
    { h: 'next run', c: (r) => (r.next_backup_at ? <span title={ts(r.next_backup_at)}>{ago(r.next_backup_at)}</span> : <span className="mute">—</span>) },
    { h: 'kept', c: (r) => r.backups_kept ?? '—' },
    { h: 'restore test', wide: true, c: (r) => (r.last_verify ? <Verify text={r.last_verify} /> : <span className="mute">not yet</span>) },
    { h: 'schedule', c: (r) => <span className="mute">{r.schedule}</span> },
    {
      h: 'problem',
      wide: true,
      c: (r) => (r.last_error && tone(r.state) === 'bad' ? <span className="bad">{r.last_error}</span> : r.last_error ? <span className="mute">{r.last_error}</span> : ''),
    },
  ];
  return (
    <div className="card">
      <div className="row spread">
        <h2>Databases</h2>
        <span className="small">
          <b>{backed}</b> of {dbs.length} backed up
          {failing > 0 && (
            <>
              {' '}
              · <b className="bad">{failing} failing</b>
            </>
          )}
        </span>
      </div>
      <Table rows={dbs} cols={cols} empty="no databases yet (the worker lists them on its next poll)" rowClass={(r) => (tone(r.state) === 'bad' ? 'row-bad' : '')} />
    </div>
  );
}

function Queue() {
  const { gen, session, confirm, profileFlag } = useApp();
  const q = useApi<{ jobs: QueueJob[]; slots: Json }>('/api/queue', [gen], 5000);
  const cancel = (j: QueueJob) =>
    confirm({
      title: `Cancel queued ${j.kind} job ${j.job_id}?`,
      body: (
        <p>
          The {j.kind} of <b>{j.database}</b> has not started; cancelling it means it is simply not taken. Nothing else changes.
        </p>
      ),
      cli: `pgbx jobs cancel ${j.job_id} --db ${j.database} --yes${profileFlag}`,
      confirm: 'Cancel the job',
      run: () => api('/api/action/cancel', { db: j.database, job_id: j.job_id }),
    });
  const canCancel = session.actions.includes('cancel');
  return (
    <div className="card">
      <div className="row spread">
        <h2>Job queue</h2>
        {q.data && (
          <span className="mute small">
            slots {q.data.slots?.max_concurrent_jobs ?? '?'} · restore lane {q.data.slots?.restore_lane ?? '?'}
          </span>
        )}
      </div>
      {q.error ? (
        <p className="bad">{q.error}</p>
      ) : !q.data ? (
        <p className="mute">loading…</p>
      ) : (
        <Table<QueueJob>
          rows={q.data.jobs}
          empty="nothing running or queued"
          cols={[
            { h: 'database', c: (j) => j.database },
            { h: 'job', c: (j) => `${j.kind} #${j.job_id}` },
            { h: 'state', c: (j) => <Badge>{j.state}</Badge> },
            {
              h: 'where',
              c: (j) => (j.position != null ? `#${j.position} in line` : j.slot === 0 ? 'restore lane' : j.slot != null ? `slot ${j.slot}` : '—'),
            },
            {
              h: 'progress / ETA',
              wide: true,
              c: (j) => (
                <>
                  {j.est_bytes && j.done_bytes != null && j.state === 'running' ? (
                    <progress max={j.est_bytes} value={Math.min(j.done_bytes, j.est_bytes)} />
                  ) : null}
                  <span>{j.progress || j.detail || ''}</span>
                  {j.eta_finish && <span className="mute"> · done ~{ts(j.eta_finish)}</span>}
                </>
              ),
            },
            {
              h: '',
              c: (j) =>
                canCancel && j.state === 'queued' ? (
                  <button type="button" className="btn small danger-ghost" onClick={() => cancel(j)}>
                    Cancel
                  </button>
                ) : null,
            },
          ]}
        />
      )}
    </div>
  );
}

function LoadGate() {
  const { gen } = useApp();
  const l = useApi<{ sample: Json; settings: Json; databases: Json[] }>('/api/load', [gen], 15000);
  if (l.error)
    return (
      <div className="card">
        <h2>Load gate</h2>
        <p className="bad">{l.error}</p>
      </div>
    );
  if (!l.data)
    return (
      <div className="card">
        <h2>Load gate</h2>
        <p className="mute">loading…</p>
      </div>
    );
  const s = l.data.sample || {};
  const st = l.data.settings || {};
  const busy = s.load_busy === true ? 'busy' : s.load_busy === false ? 'quiet' : 'no sample yet';
  return (
    <div className="card">
      <div className="row spread">
        <h2>Load gate</h2>
        <span>
          gate <Badge t={st.load_gate === 'on' ? 'warn' : 'mute'}>{st.load_gate ?? '?'}</Badge>
        </span>
      </div>
      <p>
        Server is <Badge t={s.load_busy ? 'warn' : s.load_busy === false ? 'ok' : 'mute'}>{busy}</Badge>
        {s.load_busy && s.load_reasons ? <span className="warn"> {s.load_reasons}</span> : null}
        <span className="mute small">
          {' '}
          · {s.load_active ?? '?'} active · {s.load_tps ?? '?'} tps · sampled {ago(s.load_at)}
        </span>
      </p>
      <p className="mute small">
        Busy when active &gt; {st.busy_active_backends}, tps &gt; {st.busy_tps}, writer open &gt; {st.busy_long_xact}, load/core &gt; {st.busy_loadavg}; a deferred
        backup still runs by max_defer ({st.max_defer}).
      </p>
      <Table<Json>
        rows={l.data.databases}
        empty="no databases"
        cols={[
          { h: 'database', c: (d) => d.database },
          { h: 'gate', c: (d) => d.load_gate },
          { h: 'would wait (7d)', c: (d) => d.would_defer_7d ?? 0 },
          { h: 'deferred', c: (d) => d.deferred_7d ?? 0 },
          { h: 'forced', c: (d) => d.forced_7d ?? 0 },
        ]}
      />
    </div>
  );
}

function QuietWindow({ o }: { o: Overview }) {
  const { profileFlag } = useApp();
  // start on the database whose schedule sits furthest above its quietest window
  const best = useMemo(() => {
    const w = (o.windows || []).filter((x) => x.window_cron && x.current_score != null && x.window_score != null);
    w.sort((a, b) => (b.current_score! - b.window_score!) - (a.current_score! - a.window_score!));
    return w[0]?.database ?? o.databases[0]?.database;
  }, [o]);
  const [db, setDb] = useState(best);
  const [sug, setSug] = useState<{ suggestion: Suggestion; cli: string } | null>(null);
  const [err, setErr] = useState<string | null>(null);
  useEffect(() => {
    if (!db) return;
    setSug(null);
    setErr(null);
    api<{ suggestion: Suggestion; cli: string }>(`/api/suggest/${encodeURIComponent(db)}`).then(setSug, (e) => setErr((e as Error).message));
  }, [db]);
  const w = sug?.suggestion;
  return (
    <div className="card">
      <div className="row spread">
        <h2>Quietest time to back up</h2>
        <select value={db} onChange={(e) => setDb(e.target.value)} aria-label="database">
          {o.databases.map((d) => (
            <option key={d.database}>{d.database}</option>
          ))}
        </select>
      </div>
      {err && <p className="bad">{err}</p>}
      {!w && !err && <p className="mute">loading…</p>}
      {w && !w.cron && <p className="mute">{w.start_at}</p>}
      {w && w.cron && (
        <>
          <p>
            <b>{w.start_at}</b> <code>{w.cron}</code>
          </p>
          <p className="small">
            {fmt(w.score)}× average activity there, vs {fmt(w.current_score)}× for the current schedule ({w.current_schedule}) · {w.confidence} confidence,{' '}
            {w.days_sampled} days sampled · a backup takes ~{w.est_duration}
          </p>
          <p className="mute small">Never applied by itself, and this page does not apply it: copy the call and run it yourself.</p>
          {w.apply_sql && <CommandLine cmd={w.apply_sql} label="Copy SQL" />}
          <CommandLine cmd={`${sug!.cli}${profileFlag}`} label="Copy CLI" />
        </>
      )}
    </div>
  );
}

function fmt(n: number | null | undefined): string {
  return n == null ? '?' : (Math.round(n * 100) / 100).toString();
}
