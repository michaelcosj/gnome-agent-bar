import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import St from 'gi://St';

import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

import * as fmt from './format.js';

const LEVELS = ['normal', 'warning', 'critical'];
const TICK_SECONDS = 60;
const DIM_OPACITY = 160;

// Ring (top bar) or bar (menu) gauge. Color comes from CSS per level.
const Meter = GObject.registerClass(
class AgentBarMeter extends St.DrawingArea {
    _init(shape, styleClass) {
        super._init({style_class: styleClass, y_align: Clutter.ActorAlign.CENTER});
        this._shape = shape;
        this._pct = 0;
    }

    setValue(pct, lvl) {
        this._pct = Math.min(100, Math.max(0, pct ?? 0));
        for (const l of LEVELS)
            this.remove_style_class_name(l);
        this.add_style_class_name(lvl);
        this.queue_repaint();
    }

    vfunc_repaint() {
        const cr = this.get_context();
        const [w, h] = this.get_surface_size();
        const c = this.get_theme_node().get_foreground_color();
        const [r, g, b] = [c.red / 255, c.green / 255, c.blue / 255];
        const frac = this._pct / 100;

        if (this._shape === 'ring') {
            const lw = Math.max(2, Math.round(w / 6));
            const radius = Math.min(w, h) / 2 - lw / 2;
            const start = -Math.PI / 2;
            cr.setLineWidth(lw);
            cr.setSourceRGBA(r, g, b, 0.3);
            cr.arc(w / 2, h / 2, radius, 0, 2 * Math.PI);
            cr.stroke();
            if (frac > 0) {
                cr.setSourceRGBA(r, g, b, 1);
                cr.arc(w / 2, h / 2, radius, start, start + 2 * Math.PI * frac);
                cr.stroke();
            }
        } else {
            roundedRect(cr, 0, 0, w, h);
            cr.setSourceRGBA(r, g, b, 0.2);
            cr.fill();
            if (frac > 0) {
                roundedRect(cr, 0, 0, Math.max(h, w * frac), h);
                cr.setSourceRGBA(r, g, b, 1);
                cr.fill();
            }
        }
        cr.$dispose();
    }
});

function roundedRect(cr, x, y, w, h) {
    const rad = h / 2;
    cr.newSubPath();
    cr.arc(x + w - rad, y + rad, rad, -Math.PI / 2, Math.PI / 2);
    cr.arc(x + rad, y + rad, rad, Math.PI / 2, 3 * Math.PI / 2);
    cr.closePath();
}

export const Indicator = GObject.registerClass(
class AgentBarIndicator extends PanelMenu.Button {
    _init(settings) {
        super._init(0.5, 'Agent Bar');
        this._settings = settings;
        this._snapshot = null;
        this._daemonUp = false;
        this._onRefresh = null;
        this._onPrefs = null;
        this._renderId = 0;

        const box = new St.BoxLayout({style_class: 'panel-status-menu-box'});
        this._ring = new Meter('ring', 'agent-bar-ring');
        this._label = new St.Label({
            text: '–',
            style_class: 'agent-bar-label',
            y_align: Clutter.ActorAlign.CENTER,
        });
        box.add_child(this._ring);
        box.add_child(this._label);
        this.add_child(box);

        this._content = new PopupMenu.PopupMenuSection();
        this.menu.addMenuItem(this._content);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._refreshItem = this.menu.addAction('Refresh', () => this._onRefresh?.());
        this.menu.addAction('Settings', () => this._onPrefs?.());
        this._footer = new PopupMenu.PopupMenuItem('', {reactive: false, style_class: 'agent-bar-footer'});
        this._footer.label.opacity = DIM_OPACITY;
        this.menu.addMenuItem(this._footer);

        this._settingsIds = ['panel-limit', 'stats-range', 'show-remaining', 'show-countdown']
            .map(key => this._settings.connect(`changed::${key}`, () => this._queueRender()));
        this.menu.connect('open-state-changed', (_menu, open) => {
            if (open)
                this._render();
        });
        // Countdowns and "updated … ago" age even when the data doesn't change.
        this._tickId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, TICK_SECONDS, () => {
            this._render();
            return GLib.SOURCE_CONTINUE;
        });

        this._render();
    }

    setRefreshHandler(fn) {
        this._onRefresh = fn;
    }

    setPrefsHandler(fn) {
        this._onPrefs = fn;
    }

    setDaemonUp(up) {
        this._daemonUp = up;
        this._render();
    }

    setSnapshot(snapshot) {
        this._snapshot = snapshot;
        this._render();
    }

    // Deferred so a range button can't be destroyed inside its own click handler.
    _queueRender() {
        if (this._renderId)
            return;
        this._renderId = GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
            this._renderId = 0;
            this._render();
            return GLib.SOURCE_REMOVE;
        });
    }

    _render() {
        // The Shell can destroy us (e.g. on shutdown) before disable() drops late D-Bus callbacks.
        if (this._destroyed)
            return;
        const providers = this._daemonUp ? this._snapshot?.providers ?? [] : [];
        this._renderPanel(providers[0]);
        this._renderMenu(providers);
    }

    _renderPanel(p) {
        const limit = fmt.pickLimit(p?.limits, this._settings.get_string('panel-limit'));
        if (!limit) {
            this._label.text = this._daemonUp ? '…' : '–';
            this._ring.setValue(0, 'normal');
            this._ring.opacity = 128;
            return;
        }
        const {pct} = fmt.shown(limit, this._settings.get_boolean('show-remaining'));
        let text = fmt.percent(pct);
        const until = fmt.untilReset(limit.resets_at);
        if (until && this._settings.get_boolean('show-countdown'))
            text += ` · ${until}`;
        this._label.text = text;
        this._ring.setValue(pct, fmt.level(limit));
        this._ring.opacity = p.status === 'ok' ? 255 : 128;
    }

    _renderMenu(providers) {
        this._content.removeAll();

        if (!this._daemonUp) {
            this._heading(this._content, 'Daemon not running');
            this._row(this._content, 'Start it with: systemctl --user start agent-bar', null, 'dim');
            this._refreshItem.visible = false;
            this._footer.visible = false;
            return;
        }
        this._refreshItem.visible = true;
        this._footer.visible = true;
        if (!providers.length) {
            this._row(this._content, 'Waiting for data…', null, 'dim');
            this._footer.label.text = '';
            return;
        }

        for (const p of providers)
            this._renderProvider(p);

        const p = providers[0];
        this._footer.label.text = p.status === 'ok' || !p.error
            ? `Limits updated ${fmt.ago(p.limits_updated_at)}`
            : `${p.error} · last data ${fmt.ago(p.limits_updated_at)}`;
    }

    _renderProvider(p) {
        const section = this._content;
        const plan = p.plan ? ` · ${p.plan[0].toUpperCase()}${p.plan.slice(1)}` : '';
        this._heading(section, `${p.name}${plan}`, p.status !== 'ok' ? p.status : null);

        for (const limit of p.limits)
            this._limitItem(section, limit);

        // Usage stats for the selected range.
        section.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        const range = this._settings.get_string('stats-range');
        this._rangeSwitcher(section, range);
        const s = p.stats?.[range];
        if (s) {
            this._row(section, 'Est. cost', fmt.cost(s.cost_usd), 'strong');
            this._row(section, 'Tokens', fmt.tokens(s.total_tokens), 'strong');
            this._row(section, 'Input / output', `${fmt.tokens(s.input)} / ${fmt.tokens(s.output)}`, 'sub');
            this._row(section, 'Cache read / write', `${fmt.tokens(s.cache_read)} / ${fmt.tokens(s.cache_write)}`, 'sub');
            this._row(section, 'Requests · sessions', `${s.requests} · ${s.sessions}`, 'sub');
        }
        if (s?.by_model.length) {
            section.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('By model'));
            for (const m of s.by_model.slice(0, 3))
                this._row(section, fmt.shortModel(m.model), `${fmt.cost(m.cost_usd)} · ${fmt.tokens(m.total_tokens)}`, 'sub');
        }

        const shares = (p.breakdown ?? []).filter(r => r.pct > 0).sort((a, b) => b.pct - a.pct);
        if (shares.length) {
            section.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('This week by product'));
            for (const r of shares)
                this._row(section, r.label, fmt.percent(r.pct), 'sub');
        }
    }

    _limitItem(section, limit) {
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false, style_class: 'agent-bar-limit'});
        const col = new St.BoxLayout({orientation: Clutter.Orientation.VERTICAL, x_expand: true});

        // Bars drain as quota is used; color still tracks usage level.
        const {pct, text} = fmt.shown(limit, this._settings.get_boolean('show-remaining'));
        const header = new St.BoxLayout({x_expand: true});
        header.add_child(new St.Label({text: limit.label, x_expand: true}));
        header.add_child(new St.Label({text, style_class: 'agent-bar-value'}));
        col.add_child(header);

        const bar = new Meter('bar', 'agent-bar-bar');
        bar.x_expand = true;
        bar.setValue(pct, fmt.level(limit));
        col.add_child(bar);

        const until = fmt.untilReset(limit.resets_at);
        if (until)
            col.add_child(new St.Label({text: `Resets in ${until}`, style_class: 'agent-bar-dim', opacity: DIM_OPACITY}));

        item.add_child(col);
        section.addMenuItem(item);
    }

    _rangeSwitcher(section, current) {
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        const box = new St.BoxLayout({style_class: 'agent-bar-ranges', x_expand: true});
        for (const [id, label] of fmt.RANGES) {
            const btn = new St.Button({label, style_class: 'agent-bar-range button', can_focus: true, x_expand: true});
            if (id === current)
                btn.add_style_pseudo_class('checked');
            btn.connect('clicked', () => this._settings.set_string('stats-range', id));
            box.add_child(btn);
        }
        item.add_child(box);
        section.addMenuItem(item);
    }

    _heading(section, text, badge = null) {
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        item.add_child(new St.Label({text, style_class: 'agent-bar-heading', x_expand: true}));
        if (badge)
            item.add_child(new St.Label({text: badge, style_class: 'agent-bar-badge'}));
        section.addMenuItem(item);
    }

    _row(section, left, right, kind = '') {
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false, style_class: `agent-bar-row ${kind}`});
        const dim = kind === 'sub' || kind === 'dim';
        item.add_child(new St.Label({text: left, x_expand: true, style_class: 'agent-bar-key', opacity: dim ? DIM_OPACITY : 255}));
        if (right !== null)
            item.add_child(new St.Label({text: right, style_class: 'agent-bar-value'}));
        section.addMenuItem(item);
    }

    _onDestroy() {
        this._destroyed = true;
        if (this._tickId)
            GLib.source_remove(this._tickId);
        if (this._renderId)
            GLib.source_remove(this._renderId);
        this._tickId = this._renderId = 0;
        this._settingsIds.forEach(id => this._settings.disconnect(id));
        this._settingsIds = [];
        this._settings = null;
        this._onRefresh = null;
        this._onPrefs = null;
        super._onDestroy();
    }
});
