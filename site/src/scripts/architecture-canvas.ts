/**
 * Draws `product.architecture` into a splash canvas.
 * One comet travels each edge. An arrowhead stays put, so a still frame has a direction.
 * A word on an edge sits beside the line. Nodes drift a pixel or two.
 * prefers-reduced-motion draws a single still frame and stops.
 * The canvas is aria-hidden: the page text already says this.
 */

type NodeSpec = {
	id: string;
	label: string;
	x: number;
	y: number;
	labelAt?: 'above' | 'below';
	labelAlign?: 'left' | 'center' | 'right';
	note?: string;
};
type EdgeSpec = { from: string; to: string; label?: string };
type Spec = {
	nodes: NodeSpec[];
	edges: EdgeSpec[];
	ink: string;
	signal: string;
	inkLight: string;
	signalLight: string;
};

const CYCLE_MS = 18000;

export function mountArchitecture(root: ParentNode = document): void {
	const canvas = root.querySelector<HTMLCanvasElement>('canvas.arch-canvas');
	if (!canvas || canvas.dataset.ready === '1') return;
	const raw = canvas.dataset.arch;
	if (!raw) return;
	let spec: Spec;
	try {
		spec = JSON.parse(raw) as Spec;
	} catch {
		return;
	}
	canvas.dataset.ready = '1';

	const ctx = canvas.getContext('2d');
	if (!ctx) return;

	const motion = matchMedia('(prefers-reduced-motion: reduce)');
	const byId = new Map(spec.nodes.map((n) => [n.id, n]));
	let raf = 0;
	let visible = true;

	const cssSize = () => {
		const r = canvas.getBoundingClientRect();
		return { w: r.width, h: r.height };
	};

	const resize = () => {
		const { w, h } = cssSize();
		const dpr = Math.min(window.devicePixelRatio || 1, 1.5);
		const bw = Math.max(1, Math.floor(w * dpr));
		const bh = Math.max(1, Math.floor(h * dpr));
		if (canvas.width !== bw || canvas.height !== bh) {
			canvas.width = bw;
			canvas.height = bh;
		}
		return { w, h, dpr };
	};

	const light = () => document.documentElement.dataset.theme === 'light';

	const fade = (hex: string, alpha: number) => {
		const n = hex.replace('#', '');
		const r = parseInt(n.slice(0, 2), 16);
		const g = parseInt(n.slice(2, 4), 16);
		const b = parseInt(n.slice(4, 6), 16);
		return `rgba(${r}, ${g}, ${b}, ${alpha})`;
	};

	const draw = (now: number) => {
		const { w, h, dpr } = resize();
		if (w < 8 || h < 8) return;
		const reduce = motion.matches;
		const ink = light() ? spec.inkLight : spec.ink;
		const signal = light() ? spec.signalLight : spec.signal;
		ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
		ctx.clearRect(0, 0, w, h);
		ctx.lineCap = 'round';
		ctx.font = '11px ui-sans-serif, system-ui, sans-serif';
		ctx.textAlign = 'center';

		const at = (n: NodeSpec) => {
			const bob = reduce ? 0 : Math.sin(now / 2800 + n.x * 6) * 0.7;
			return { x: n.x * w, y: n.y * h + bob };
		};

		for (let i = 0; i < spec.edges.length; i++) {
			const edge = spec.edges[i];
			const a = byId.get(edge.from);
			const b = byId.get(edge.to);
			if (!a || !b) continue;
			const p = at(a);
			const q = at(b);
			const ang = Math.atan2(q.y - p.y, q.x - p.x);
			const len = Math.hypot(q.x - p.x, q.y - p.y);
			if (len < 1) continue;
			const tip = { x: q.x - Math.cos(ang) * 8, y: q.y - Math.sin(ang) * 8 };

			ctx.beginPath();
			ctx.moveTo(p.x, p.y);
			ctx.lineTo(tip.x, tip.y);
			ctx.strokeStyle = ink;
			ctx.globalAlpha = 0.4;
			ctx.lineWidth = 1;
			ctx.setLineDash([]);
			ctx.stroke();

			ctx.globalAlpha = 0.75;
			ctx.fillStyle = ink;
			const head = 6;
			ctx.beginPath();
			ctx.moveTo(tip.x, tip.y);
			ctx.lineTo(tip.x - head * Math.cos(ang - 0.4), tip.y - head * Math.sin(ang - 0.4));
			ctx.lineTo(tip.x - head * Math.cos(ang + 0.4), tip.y - head * Math.sin(ang + 0.4));
			ctx.closePath();
			ctx.fill();

			if (edge.label) {
				ctx.font = '10px ui-sans-serif, system-ui, sans-serif';
				ctx.globalAlpha = 0.9;
				ctx.fillStyle = ink;
				ctx.fillText(edge.label, (p.x + tip.x) / 2 - Math.sin(ang) * 11, (p.y + tip.y) / 2 + Math.cos(ang) * 11);
			}

			if (reduce) continue;
			const u = ((now / CYCLE_MS + i / spec.edges.length) % 1);
			ctx.beginPath();
			ctx.moveTo(p.x, p.y);
			ctx.lineTo(tip.x, tip.y);
			ctx.strokeStyle = signal;
			ctx.globalAlpha = 0.85;
			ctx.lineWidth = 1.25;
			ctx.setLineDash([Math.max(8, len * 0.08), len]);
			ctx.lineDashOffset = -u * len;
			ctx.stroke();
			ctx.setLineDash([]);
		}

		ctx.globalAlpha = 1;
		for (const n of spec.nodes) {
			const p = at(n);
			const glow = ctx.createRadialGradient(p.x, p.y, 0, p.x, p.y, 9);
			glow.addColorStop(0, fade(signal, 0.4));
			glow.addColorStop(1, fade(signal, 0));
			ctx.globalAlpha = reduce ? 0.16 : 0.2;
			ctx.fillStyle = glow;
			ctx.beginPath();
			ctx.arc(p.x, p.y, 9, 0, Math.PI * 2);
			ctx.fill();

			ctx.globalAlpha = 0.9;
			ctx.fillStyle = ink;
			ctx.beginPath();
			ctx.arc(p.x, p.y, 2, 0, Math.PI * 2);
			ctx.fill();

			ctx.font = '11px ui-sans-serif, system-ui, sans-serif';
			ctx.globalAlpha = 0.8;
			ctx.fillStyle = ink;
			const ly = n.labelAt === 'above' ? p.y - 10 : p.y + 14;
			const align = n.labelAlign ?? 'center';
			ctx.textAlign = align;
			const lx = align === 'right' ? p.x - 10 : align === 'left' ? p.x + 10 : p.x;
			ctx.fillText(n.label, lx, ly);
			ctx.textAlign = 'center';
			if (n.note) {
				ctx.font = '10px ui-sans-serif, system-ui, sans-serif';
				ctx.globalAlpha = 0.55;
				ctx.fillText(n.note, p.x, ly + 12);
			}
		}
		ctx.globalAlpha = 1;
	};

	const stop = () => {
		cancelAnimationFrame(raf);
		raf = 0;
	};

	const tick = (now: number) => {
		raf = 0;
		const { w, h } = cssSize();
		const run = visible && !document.hidden && !motion.matches && w >= 8 && h >= 8;
		draw(now);
		if (run) raf = requestAnimationFrame(tick);
	};

	const sync = () => {
		const { w, h } = cssSize();
		const run = visible && !document.hidden && !motion.matches && w >= 8 && h >= 8;
		if (run && !raf) raf = requestAnimationFrame(tick);
		if (!run) {
			stop();
			draw(0);
		}
	};

	const io = new IntersectionObserver(([entry]) => {
		visible = entry?.isIntersecting ?? false;
		sync();
	});
	io.observe(canvas);
	document.addEventListener('visibilitychange', sync);
	motion.addEventListener('change', sync);
	new MutationObserver(sync).observe(document.documentElement, {
		attributes: true,
		attributeFilter: ['data-theme'],
	});
	new ResizeObserver(sync).observe(canvas);
	sync();
}
