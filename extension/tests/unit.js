// Unit tests for the extension's pure modules: gjs -m extension/tests/unit.js
import * as f from '../agent-bar@local/format.js';
import {dueAlerts} from '../agent-bar@local/alerts.js';

const eq = (a, b) => {
    if (a !== b)
        throw new Error(`${JSON.stringify(a)} !== ${JSON.stringify(b)}`);
};

eq(f.tokens(124101107), '124M'); eq(f.tokens(2239905), '2.2M'); eq(f.tokens(1744), '1.7K'); eq(f.tokens(12), '12'); eq(f.tokens(1000000), '1M');
eq(f.cost(50.2313), '$50.23'); eq(f.cost(0.001), '<$0.01'); eq(f.cost(0), '$0.00');
eq(f.duration(45*60e3), '45m'); eq(f.duration((2*60+13)*60e3), '2h13m'); eq(f.duration((3*24+4)*3600e3), '3d4h'); eq(f.duration(-5), '0m');
eq(f.level({used_pct: 50, severity: 'normal'}), 'normal'); eq(f.level({used_pct: 75}), 'warning'); eq(f.level({used_pct: 95, severity: 'normal'}), 'critical'); eq(f.level({used_pct: 10, severity: 'warning'}), 'warning');
const L = [{id: 'session', used_pct: 11}, {id: 'weekly_all', used_pct: 25}];
eq(f.pickLimit(L, 'auto').id, 'weekly_all'); eq(f.pickLimit(L, 'session').id, 'session'); eq(f.pickLimit(L, 'nope').id, 'weekly_all'); eq(f.pickLimit([], 'auto'), null);
eq(f.ago(new Date(Date.now() - 5000).toISOString()), 'just now'); eq(f.ago(new Date(Date.now() - 125*60e3).toISOString()), '2h05m ago');
eq(f.shortModel('claude-opus-5-5'), 'opus-5-5');
// ChatGPT windows map onto the same panel-limit choices.
const G = [{id: 'primary', used_pct: 40}, {id: 'secondary', used_pct: 10}];
eq(f.pickLimit(G, 'session').id, 'primary'); eq(f.pickLimit(G, 'weekly_all').id, 'secondary');
// Top bar: every shown account with limits; the closest one is marked.
const C = {id: 'claude', limits: L}, O = {id: 'openai', limits: G}, U = {id: 'x', limits: []};
let pe = f.panelEntries([C, O, U]);
eq(pe.map(e => `${e.provider.id}:${e.closest}`).join(), 'claude:false,openai:true');
eq(f.panelEntries([C, O], {mode: 'closest'}).map(e => e.provider.id).join(), 'openai');
eq(f.panelEntries([C, O], {hidden: ['openai']}).map(e => `${e.provider.id}:${e.closest}`).join(), 'claude:true');
eq(f.panelEntries([{...C, limit_reached: true}, O]).find(e => e.closest).provider.id, 'claude');
eq(f.panelEntries([U]).length, 0);
eq(f.selectedAccount([C, O], 'openai').id, 'openai'); eq(f.selectedAccount([C, O], 'gone').id, 'claude'); eq(f.selectedAccount([], ''), null);
eq(f.remaining({used_pct: 25}), 75); eq(f.remaining({used_pct: 104}), 0); eq(f.shown({used_pct: 25}, true).text, '75% left'); eq(f.shown({used_pct: 25}, false).text, '25% used');

// Alerts: one per window at the most severe level crossed; resets_at jitter ignored.
const T = {warnLeft: 20, criticalLeft: 5};
const prov = (used, reset = '2026-10-11T21:00:00.385Z') =>
    [{id: 'claude', name: 'Claude', limits: [{id: 'session', label: '5-hour', used_pct: used, resets_at: reset}]}];
const sent = new Set();
eq(dueAlerts(prov(70), T, sent).length, 0);
let a = dueAlerts(prov(82), T, sent);
eq(a.length, 1); eq(a[0].level, 'warning'); eq(a[0].left, 18);
eq(dueAlerts(prov(85, '2026-10-11T20:59:59.616Z'), T, sent).length, 0);
a = dueAlerts(prov(96), T, sent);
eq(a.length, 1); eq(a[0].level, 'critical');
eq(dueAlerts(prov(99), T, sent).length, 0);
eq(dueAlerts(prov(85, '2026-10-12T02:00:00Z'), T, sent).length, 1); // next window
const fresh = new Set();
a = dueAlerts(prov(97), T, fresh); // start already critical: one alert, no later warning
eq(a.length, 1); eq(a[0].level, 'critical');
eq(dueAlerts(prov(90), T, fresh).length, 0);
eq(dueAlerts(undefined, T, new Set()).length, 0);

print('extension unit tests: all passed');

// Hourly chart caption.
eq(f.hourlySummary([{cost_usd: 0, start: '2026-10-11T10:00:00Z'}]), null);
eq(f.hourlySummary(undefined), null);
const hs = f.hourlySummary([
    {cost_usd: 1.5, start: '2026-10-11T10:00:00Z'},
    {cost_usd: 3.25, start: '2026-10-11T11:00:00Z'},
]);
eq(hs.total, '$4.75');
eq(hs.peak, `peak $3.25 at ${f.hourLabel('2026-10-11T11:00:00Z')}`);
eq(/^\d\d:00$/.test(f.hourLabel('2026-10-11T11:00:00Z')), true);
print('hourly chart tests: all passed');
