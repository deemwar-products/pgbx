/**
 * What this docs site is, for the splash canvas and the theme.
 * Doc pages do not read this. The agent skill `docs-product-skin` does.
 *
 * Dark mode paints the primary button and the current sidebar item with
 * `high`, and the text on them with `low`. Light mode paints that same
 * pair from `mid` against the light page color. `surface` is the gray
 * ramp in the same hue, so the page and the accent are one theme.
 */
export const product = {
	id: 'pgbx',
	sentence:
		'A database is created, pgbx backs it up to S3, and a restore comes back under a new name even if Postgres is down.',
	colors: {
		dark: {
			low: 'hsl(26, 28%, 12%)',
			mid: 'hsl(28, 52%, 46%)',
			high: 'hsl(32, 64%, 64%)',
			white: 'hsl(36, 22%, 96%)',
			gray1: 'hsl(36, 14%, 90%)',
			gray2: 'hsl(36, 8%, 76%)',
			gray3: 'hsl(34, 6%, 56%)',
			gray4: 'hsl(32, 6%, 36%)',
			gray5: 'hsl(30, 8%, 22%)',
			gray6: 'hsl(28, 10%, 13%)',
			black: 'hsl(28, 12%, 8%)',
		},
		light: {
			low: 'hsl(36, 45%, 90%)',
			mid: 'hsl(24, 52%, 30%)',
			high: 'hsl(22, 40%, 18%)',
			white: 'hsl(30, 14%, 14%)',
			gray1: 'hsl(30, 12%, 18%)',
			gray2: 'hsl(30, 8%, 26%)',
			gray3: 'hsl(32, 6%, 38%)',
			gray4: 'hsl(32, 5%, 52%)',
			gray5: 'hsl(34, 8%, 74%)',
			gray6: 'hsl(36, 18%, 92%)',
			gray7: 'hsl(40, 28%, 97%)',
			black: 'hsl(40, 33%, 99%)',
		},
		ink: '#c4b8ac',
		signal: '#dea768',
		inkLight: '#5c5148',
		signalLight: '#8a4e1c',
	},
	/**
	 * 0–1 inside the small canvas on the right of the splash, not the
	 * whole hero. Keep labels inset so they are not clipped.
	 */
	architecture: {
		nodes: [
			{ id: 'pg', label: 'Postgres (pgbx inside)', x: 0.38, y: 0.5, labelAt: 'above', labelAlign: 'right' },
			{ id: 's3', label: 'S3', x: 0.58, y: 0.28, labelAt: 'above' },
			{ id: 'cli', label: 'CLI', x: 0.58, y: 0.76, labelAt: 'below' },
			{ id: 'name', label: 'new name', x: 0.86, y: 0.42, labelAt: 'below' },
		],
		edges: [
			{ from: 'pg', to: 's3', label: 'backs up' },
			{ from: 's3', to: 'name', label: 'SQL' },
			{ from: 's3', to: 'cli', label: 'server down' },
			{ from: 'cli', to: 'name', label: 'restore' },
		],
	},
} as const;

export type Product = typeof product;

function decl(tokens: Record<string, string>): string {
	return Object.entries(tokens)
		.map(([name, value]) => `${name}:${value}`)
		.join(';');
}

export function accentCss(p: Product = product): string {
	const d = p.colors.dark;
	const l = p.colors.light;
	const dark = decl({
		'--sl-color-white': d.white,
		'--sl-color-gray-1': d.gray1,
		'--sl-color-gray-2': d.gray2,
		'--sl-color-gray-3': d.gray3,
		'--sl-color-gray-4': d.gray4,
		'--sl-color-gray-5': d.gray5,
		'--sl-color-gray-6': d.gray6,
		'--sl-color-black': d.black,
		'--sl-color-accent-low': d.low,
		'--sl-color-accent': d.mid,
		'--sl-color-accent-high': d.high,
	});
	const light = decl({
		'--sl-color-white': l.white,
		'--sl-color-gray-1': l.gray1,
		'--sl-color-gray-2': l.gray2,
		'--sl-color-gray-3': l.gray3,
		'--sl-color-gray-4': l.gray4,
		'--sl-color-gray-5': l.gray5,
		'--sl-color-gray-6': l.gray6,
		'--sl-color-gray-7': l.gray7,
		'--sl-color-black': l.black,
		'--sl-color-accent-low': l.low,
		'--sl-color-accent': l.mid,
		'--sl-color-accent-high': l.high,
	});
	return [
		`:root{${dark}}`,
		`:root[data-theme='light']{${light}}`,
		`.card .icon{border-color:var(--sl-color-accent);background-color:var(--sl-color-accent-low)}`,
	].join('');
}
