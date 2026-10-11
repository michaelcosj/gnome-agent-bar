// Pure formatting helpers (no GNOME imports, so they can be tested with plain gjs).

export const RANGES = [
    ['1d', '24h'],
    ['7d', '7d'],
    ['30d', '30d'],
];

export function tokens(n) {
    if (n >= 1e9)
        return `${trim(n / 1e9)}B`;
    if (n >= 1e6)
        return `${trim(n / 1e6)}M`;
    if (n >= 1e3)
        return `${trim(n / 1e3)}K`;
    return `${n}`;
}

function trim(x) {
    return x >= 100 ? x.toFixed(0) : x.toFixed(1).replace(/\.0$/, '');
}

export function cost(usd) {
    if (usd > 0 && usd < 0.01)
        return '<$0.01';
    return `$${usd.toFixed(2)}`;
}

export function percent(p) {
    return `${Math.round(p)}%`;
}

export function remaining(limit) {
    return Math.max(0, 100 - limit.used_pct);
}

// The percentage to display (left or used) and its menu caption.
export function shown(limit, showRemaining) {
    return showRemaining
        ? {pct: remaining(limit), text: `${percent(remaining(limit))} left`}
        : {pct: limit.used_pct, text: `${percent(limit.used_pct)} used`};
}

// Compact duration: "45m", "2h13m", "3d4h".
export function duration(ms) {
    const mins = Math.max(0, Math.round(ms / 60000));
    if (mins < 60)
        return `${mins}m`;
    const hours = Math.floor(mins / 60);
    if (hours < 24)
        return `${hours}h${String(mins % 60).padStart(2, '0')}m`;
    return `${Math.floor(hours / 24)}d${hours % 24}h`;
}

export function untilReset(resetsAt, now = Date.now()) {
    if (!resetsAt)
        return null;
    const t = Date.parse(resetsAt);
    return Number.isNaN(t) ? null : duration(t - now);
}

export function ago(iso, now = Date.now()) {
    if (!iso)
        return 'never';
    const secs = Math.round((now - Date.parse(iso)) / 1000);
    if (secs < 60)
        return 'just now';
    return `${duration(secs * 1000)} ago`;
}

// "normal" | "warning" | "critical": our thresholds, raised by the server's severity.
export function level(limit) {
    const p = limit.used_pct;
    let lvl = p >= 90 ? 2 : p >= 70 ? 1 : 0;
    const sev = (limit.severity ?? 'normal').toLowerCase();
    if (sev !== 'normal')
        lvl = Math.max(lvl, sev === 'warning' ? 1 : 2);
    return ['normal', 'warning', 'critical'][lvl];
}

// Limit for the top bar: the configured id, else the most used.
export function pickLimit(limits, preferred) {
    if (!limits?.length)
        return null;
    const chosen = preferred !== 'auto' && limits.find(l => l.id === preferred);
    return chosen || limits.reduce((a, b) => (b.used_pct > a.used_pct ? b : a));
}

// Local clock hour for an ISO timestamp: "14:00".
export function hourLabel(iso) {
    return `${String(new Date(Date.parse(iso)).getHours()).padStart(2, '0')}:00`;
}

// Default caption for the hourly chart, or null when there's no usage.
export function hourlySummary(buckets) {
    const total = (buckets ?? []).reduce((sum, b) => sum + b.cost_usd, 0);
    if (!(total > 0))
        return null;
    const peak = buckets.reduce((a, b) => (b.cost_usd > a.cost_usd ? b : a));
    return {total: cost(total), peak: `peak ${cost(peak.cost_usd)} at ${hourLabel(peak.start)}`};
}

export function shortModel(model) {
    return model.replace(/^claude-/, '');
}
