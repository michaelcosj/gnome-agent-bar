// Decides which low-quota notifications are due. Pure, so it can be tested with plain gjs.

import {remaining} from './format.js';

// Reset times jitter by ~1s between polls; round so a window keeps one identity.
function windowKey(provider, limit) {
    const reset = limit.resets_at ? Math.round(Date.parse(limit.resets_at) / 3600e3) : '';
    return `${provider.id}/${limit.id}/${reset}`;
}

// Returns alerts not yet in `sent` (and records them): at most one per window,
// the most severe level crossed. A new window (after reset) alerts again.
export function dueAlerts(providers, {warnLeft, criticalLeft}, sent) {
    const due = [];
    for (const provider of providers ?? []) {
        for (const limit of provider.limits ?? []) {
            const left = remaining(limit);
            const level = left <= criticalLeft ? 'critical' : left <= warnLeft ? 'warning' : null;
            if (!level)
                continue;
            const key = windowKey(provider, limit);
            if (sent.has(`${key}/${level}`))
                continue;
            sent.add(`${key}/${level}`);
            if (level === 'critical')
                sent.add(`${key}/warning`);
            due.push({provider, limit, level, left});
        }
    }
    return due;
}
