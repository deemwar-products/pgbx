import { type Check } from '../api';
import { useApp } from '../App';
import { CommandLine, ErrorCard, Loading, useApi, type Tone } from '../ui';

function level(c: Check): [Tone, string] {
  if (c.info) return ['info', 'info'];
  if (c.ok) return ['ok', 'ok'];
  if (c.warning) return ['warn', 'warn'];
  return ['bad', 'FAIL'];
}

/** A fix that is a command gets a copy button; prose stays prose. */
function Fix({ fix }: { fix: string }) {
  const m = /^(pgbx [^;,(]+?|SELECT .+|ALTER .+|CREATE .+|sudo .+)$/.exec(fix.trim());
  return m ? <CommandLine cmd={fix.trim()} /> : <p className="small">fix: {fix}</p>;
}

export function HealthScreen() {
  const { gen } = useApp();
  const h = useApi<{ healthy: boolean; postgres_up: boolean; checks: Check[]; diagnosis?: { probable_cause?: string } }>('/api/health', [gen]);
  if (h.error) return <ErrorCard error={h.error} />;
  if (!h.data) return <Loading what="health checks" />;
  const checks = h.data.checks;
  const fails = checks.filter((c) => level(c)[0] === 'bad').length;
  const warns = checks.filter((c) => level(c)[0] === 'warn').length;
  return (
    <>
      <div className="card">
        <div className="row spread wrap-row">
          <h1 className={`h1 ${h.data.healthy ? 'ok' : 'bad'}`}>{h.data.healthy ? 'Healthy' : 'NOT healthy'}</h1>
          <span className="small">
            {checks.length} checks · <b className={fails ? 'bad' : ''}>{fails} failing</b> · <b className={warns ? 'warn' : ''}>{warns} warnings</b>
          </span>
        </div>
        <p className="mute small">
          The same checks as <code>pgbx doctor</code>. Fixes are shown, never run. Warnings are advice and never make the server unhealthy.
        </p>
        {h.data.diagnosis?.probable_cause && <p className="bad">Postgres is down: {h.data.diagnosis.probable_cause} (pgbx diagnose)</p>}
      </div>
      <ul className="checks">
        {checks.map((c, i) => {
          const [t, label] = level(c);
          return (
            <li key={i} className={`card check ${t}`}>
              <div className="row spread wrap-row">
                <b>{c.name}</b>
                <span className={`badge ${t}`}>{label}</span>
              </div>
              <p className="small wrap">{c.detail}</p>
              {c.info && <p className="small mute wrap">{c.info}</p>}
              {!c.ok && c.fix && <Fix fix={c.fix} />}
            </li>
          );
        })}
      </ul>
    </>
  );
}
